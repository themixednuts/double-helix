use crate::{
    AuthError, Authenticate, ClientFrame, Credential, HostFrame, HostSession, ParticipantInfo,
    Role, SessionId, MAX_TRANSPORT_FRAME_BYTES, PROTOCOL_VERSION,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use helix_ipc::FrameCodec;
use quinn::rustls::{
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    RootCertStore,
};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Mutex;

const SERVER_NAME: &str = "collab.double-helix.invalid";
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONNECT_CODE_BYTES: usize = 16 * 1024;
const MAX_CERTIFICATE_BYTES: usize = 8 * 1024;

#[derive(Clone, Serialize, Deserialize)]
struct Ticket {
    address: SocketAddr,
    session: SessionId,
    credential: Credential,
    certificate: ByteBuf,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConnectCode(String);

impl ConnectCode {
    fn from_ticket(ticket: &Ticket) -> Result<Self, TransportError> {
        let bytes = rmp_serde::to_vec_named(ticket)?;
        if bytes.len() > MAX_CONNECT_CODE_BYTES {
            return Err(TransportError::ConnectCodeTooLarge);
        }
        Ok(Self(format!(
            "dhx-collab:{}",
            URL_SAFE_NO_PAD.encode(bytes)
        )))
    }

    fn ticket(&self) -> Result<Ticket, TransportError> {
        let encoded = self
            .0
            .strip_prefix("dhx-collab:")
            .ok_or(TransportError::InvalidConnectCode)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| TransportError::InvalidConnectCode)?;
        if bytes.len() > MAX_CONNECT_CODE_BYTES {
            return Err(TransportError::ConnectCodeTooLarge);
        }
        let ticket: Ticket = rmp_serde::from_slice(&bytes)?;
        if ticket.certificate.is_empty() || ticket.certificate.len() > MAX_CERTIFICATE_BYTES {
            return Err(TransportError::InvalidCertificate);
        }
        Ok(ticket)
    }
}

impl fmt::Debug for ConnectCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConnectCode([REDACTED])")
    }
}

impl fmt::Display for ConnectCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::str::FromStr for ConnectCode {
    type Err = TransportError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let code = Self(value.to_owned());
        code.ticket()?;
        Ok(code)
    }
}

pub struct HostEndpoint {
    endpoint: quinn::Endpoint,
    advertised: SocketAddr,
    certificate: CertificateDer<'static>,
    session: Arc<Mutex<HostSession>>,
}

impl HostEndpoint {
    pub fn bind(
        bind: SocketAddr,
        mut advertised: SocketAddr,
        owner_name: impl Into<String>,
    ) -> Result<Self, TransportError> {
        let generated = rcgen::generate_simple_self_signed([SERVER_NAME.to_owned()])?;
        let certificate = generated.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der());
        let mut server = quinn::ServerConfig::with_single_cert(
            vec![certificate.clone()],
            quinn::rustls::pki_types::PrivateKeyDer::Pkcs8(key),
        )?;
        let mut transport = quinn::TransportConfig::default();
        transport
            .max_concurrent_bidi_streams(1_u8.into())
            .max_concurrent_uni_streams(0_u8.into())
            .keep_alive_interval(Some(Duration::from_secs(15)));
        server.transport_config(Arc::new(transport));
        let endpoint = quinn::Endpoint::server(server, bind)?;
        if advertised.port() == 0 {
            advertised.set_port(endpoint.local_addr()?.port());
        }
        if advertised.ip().is_unspecified() {
            return Err(TransportError::UnspecifiedAdvertisedAddress);
        }
        Ok(Self {
            endpoint,
            advertised,
            certificate,
            session: Arc::new(Mutex::new(HostSession::new(owner_name)?)),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.endpoint.local_addr().map_err(Into::into)
    }

    pub async fn owner(&self) -> crate::ParticipantId {
        self.session.lock().await.owner()
    }

    pub async fn owner_code(&self, now_unix_secs: u64) -> Result<ConnectCode, TransportError> {
        let mut session = self.session.lock().await;
        let session_id = session.id();
        let authenticated = session.issue_owner_resume(now_unix_secs)?;
        ConnectCode::from_ticket(&Ticket {
            address: self.advertised,
            session: session_id,
            credential: Credential::Resume(authenticated.resume),
            certificate: self.certificate.as_ref().to_vec().into(),
        })
    }

    pub async fn invite(
        &self,
        actor: crate::ParticipantId,
        role: Role,
        expires_unix_secs: u64,
        now_unix_secs: u64,
    ) -> Result<ConnectCode, TransportError> {
        let invitation =
            self.session
                .lock()
                .await
                .invite(actor, role, expires_unix_secs, now_unix_secs)?;
        ConnectCode::from_ticket(&Ticket {
            address: self.advertised,
            session: invitation.session,
            credential: Credential::Invite(invitation.token),
            certificate: self.certificate.as_ref().to_vec().into(),
        })
    }

    pub async fn accept(&self, now_unix_secs: u64) -> Result<Accepted, TransportError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(TransportError::EndpointClosed)?;
        let connection = tokio::time::timeout(AUTH_TIMEOUT, incoming)
            .await
            .map_err(|_| TransportError::AuthenticationTimeout)??;
        let (mut send, mut recv) = tokio::time::timeout(AUTH_TIMEOUT, connection.accept_bi())
            .await
            .map_err(|_| TransportError::AuthenticationTimeout)??;
        let mut read_codec = codec();
        let mut write_codec = codec();
        let frame =
            tokio::time::timeout(AUTH_TIMEOUT, read_codec.read::<ClientFrame, _>(&mut recv))
                .await
                .map_err(|_| TransportError::AuthenticationTimeout)??;
        let ClientFrame::Authenticate(authenticate) = frame else {
            connection.close(1_u8.into(), b"authentication required");
            return Err(TransportError::AuthenticationRequired);
        };
        let authenticated = self.session.lock().await.authenticate(
            authenticate.protocol,
            authenticate.session,
            authenticate.credential,
            authenticate.name,
            now_unix_secs,
        );
        let authenticated = match authenticated {
            Ok(authenticated) => authenticated,
            Err(error) => {
                let rejection = HostFrame::Rejected(auth_protocol_error(&error));
                let _ = write_codec.write(&mut send, &rejection).await;
                let _ = send.finish();
                connection.close(2_u8.into(), b"authentication rejected");
                return Err(error.into());
            }
        };
        write_codec
            .write(
                &mut send,
                &HostFrame::Authenticated {
                    participant: authenticated.participant.clone(),
                    resume: authenticated.resume.clone(),
                    resume_expires_unix_secs: authenticated.resume_expires_unix_secs,
                },
            )
            .await?;
        let (sender, receiver) = connection_parts(
            self.endpoint.clone(),
            connection,
            send,
            recv,
            read_codec,
            write_codec,
        );
        Ok(Accepted {
            participant: authenticated.participant,
            sender,
            receiver,
        })
    }

    pub async fn disconnect(&self, participant: &ParticipantInfo) {
        self.session
            .lock()
            .await
            .disconnect(participant.id, participant.incarnation);
    }

    pub async fn is_disconnected(&self, participant: &ParticipantInfo) -> bool {
        self.session.lock().await.is_disconnected(participant)
    }

    pub async fn authorize_request(
        &self,
        participant: crate::ParticipantId,
        incarnation: u64,
        request: &crate::Request,
    ) -> Result<(), AuthError> {
        self.session
            .lock()
            .await
            .authorize_request(participant, incarnation, request)
    }

    pub async fn participants(&self) -> Vec<ParticipantInfo> {
        self.session.lock().await.participant_infos()
    }

    pub async fn set_role(
        &self,
        actor: crate::ParticipantId,
        participant: crate::ParticipantId,
        role: Role,
    ) -> Result<(), AuthError> {
        self.session.lock().await.set_role(actor, participant, role)
    }

    pub async fn remove_participant(
        &self,
        actor: crate::ParticipantId,
        participant: crate::ParticipantId,
    ) -> Result<crate::Participant, AuthError> {
        self.session.lock().await.remove(actor, participant)
    }

    pub fn close(&self) {
        self.endpoint.close(0_u8.into(), b"host closed session");
    }
}

pub struct Connected {
    pub participant: ParticipantInfo,
    pub resume: ConnectCode,
    pub sender: ConnectionSender,
    pub receiver: ConnectionReceiver,
}

pub struct Accepted {
    pub participant: ParticipantInfo,
    pub sender: ConnectionSender,
    pub receiver: ConnectionReceiver,
}

impl Connected {
    pub async fn connect(code: &ConnectCode, name: String) -> Result<Self, TransportError> {
        if name.is_empty() {
            return Err(TransportError::InvalidParticipantName);
        }
        let ticket = code.ticket()?;
        let certificate = CertificateDer::from(ticket.certificate.clone().into_vec());
        let mut roots = RootCertStore::empty();
        roots
            .add(certificate)
            .map_err(|_| TransportError::InvalidCertificate)?;
        let mut endpoint = quinn::Endpoint::client("[::]:0".parse().expect("valid bind address"))?;
        endpoint.set_default_client_config(
            quinn::ClientConfig::with_root_certificates(Arc::new(roots))
                .map_err(|_| TransportError::InvalidCertificate)?,
        );
        let connection =
            tokio::time::timeout(AUTH_TIMEOUT, endpoint.connect(ticket.address, SERVER_NAME)?)
                .await
                .map_err(|_| TransportError::ConnectTimeout)??;
        let (mut send, mut recv) = connection.open_bi().await?;
        let mut read_codec = codec();
        let mut write_codec = codec();
        write_codec
            .write(
                &mut send,
                &ClientFrame::Authenticate(Authenticate {
                    protocol: PROTOCOL_VERSION,
                    session: ticket.session,
                    credential: ticket.credential,
                    name,
                }),
            )
            .await?;
        let response =
            tokio::time::timeout(AUTH_TIMEOUT, read_codec.read::<HostFrame, _>(&mut recv))
                .await
                .map_err(|_| TransportError::AuthenticationTimeout)??;
        let HostFrame::Authenticated {
            participant,
            resume,
            resume_expires_unix_secs: _,
        } = response
        else {
            connection.close(2_u8.into(), b"authentication rejected");
            return Err(TransportError::AuthenticationRejected);
        };
        let resume = ConnectCode::from_ticket(&Ticket {
            address: ticket.address,
            session: ticket.session,
            credential: Credential::Resume(resume),
            certificate: ticket.certificate,
        })?;
        let (sender, receiver) =
            connection_parts(endpoint, connection, send, recv, read_codec, write_codec);
        Ok(Self {
            participant,
            resume,
            sender,
            receiver,
        })
    }
}

fn connection_parts(
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    read_codec: FrameCodec,
    write_codec: FrameCodec,
) -> (ConnectionSender, ConnectionReceiver) {
    let handle = Arc::new(ConnectionHandle {
        _endpoint: endpoint,
        connection,
    });
    (
        ConnectionSender {
            stream: send,
            codec: write_codec,
            handle: handle.clone(),
        },
        ConnectionReceiver {
            stream: recv,
            codec: read_codec,
            handle,
        },
    )
}

struct ConnectionHandle {
    _endpoint: quinn::Endpoint,
    connection: quinn::Connection,
}

pub struct ConnectionSender {
    stream: quinn::SendStream,
    codec: FrameCodec,
    handle: Arc<ConnectionHandle>,
}

impl ConnectionSender {
    pub async fn send(&mut self, frame: &impl Serialize) -> Result<(), TransportError> {
        self.codec.write(&mut self.stream, frame).await?;
        Ok(())
    }

    pub fn close(&mut self) {
        let _ = self.stream.finish();
        self.handle.connection.close(0_u8.into(), b"peer closed");
    }
}

pub struct ConnectionReceiver {
    stream: quinn::RecvStream,
    codec: FrameCodec,
    handle: Arc<ConnectionHandle>,
}

impl ConnectionReceiver {
    pub async fn receive<T: serde::de::DeserializeOwned>(&mut self) -> Result<T, TransportError> {
        self.codec.read(&mut self.stream).await.map_err(Into::into)
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.handle.connection.remote_address()
    }
}

fn codec() -> FrameCodec {
    FrameCodec::with_limits(8 * 1024, MAX_TRANSPORT_FRAME_BYTES)
}

fn auth_protocol_error(error: &AuthError) -> crate::ProtocolError {
    use crate::ErrorCode;
    let code = match error {
        AuthError::ProtocolMismatch { .. } => ErrorCode::ProtocolMismatch,
        AuthError::Expired => ErrorCode::ExpiredCredential,
        AuthError::Forbidden { .. } => ErrorCode::Forbidden,
        AuthError::ParticipantLimit | AuthError::InviteLimit => ErrorCode::ResourceExhausted,
        AuthError::InvalidName => ErrorCode::InvalidRequest,
        _ => ErrorCode::InvalidCredential,
    };
    crate::ProtocolError {
        code,
        message: error.to_string(),
        retryable: false,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Frame(#[from] helix_ipc::FrameError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("failed to encode collaboration connect code: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("failed to decode collaboration connect code: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("failed to generate collaboration certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("failed to configure collaboration TLS: {0}")]
    Tls(#[from] quinn::rustls::Error),
    #[error("collaboration connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("collaboration connect failed: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("collaboration stream failed: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("collaboration connect code is invalid")]
    InvalidConnectCode,
    #[error("collaboration connect code is too large")]
    ConnectCodeTooLarge,
    #[error("collaboration certificate is invalid")]
    InvalidCertificate,
    #[error("advertised collaboration address must not be unspecified")]
    UnspecifiedAdvertisedAddress,
    #[error("collaboration endpoint is closed")]
    EndpointClosed,
    #[error("collaboration connection timed out")]
    ConnectTimeout,
    #[error("collaboration authentication timed out")]
    AuthenticationTimeout,
    #[error("collaboration authentication must be the first frame")]
    AuthenticationRequired,
    #[error("collaboration authentication was rejected")]
    AuthenticationRejected,
    #[error("collaboration participant name is invalid")]
    InvalidParticipantName,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pinned_quic_invite_authenticates_once_and_resumes_once() {
        let host = Arc::new(
            HostEndpoint::bind(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1:0".parse().unwrap(),
                "owner",
            )
            .unwrap(),
        );
        let owner = host.owner().await;
        let invite = host.invite(owner, Role::Write, 100, 10).await.unwrap();

        let accepting = {
            let host = host.clone();
            tokio::spawn(async move { host.accept(11).await.unwrap() })
        };
        let guest = Connected::connect(&invite, "guest".to_owned())
            .await
            .unwrap();
        let hosted = accepting.await.unwrap();
        assert_eq!(guest.participant.id, hosted.participant.id);
        assert_eq!(guest.participant.role, Role::Write);
        let old_resume = guest.resume.clone();
        host.disconnect(&hosted.participant).await;
        drop(guest);
        drop(hosted);

        let accepting = {
            let host = host.clone();
            tokio::spawn(async move { host.accept(12).await.unwrap() })
        };
        let resumed = Connected::connect(&old_resume, "guest".to_owned())
            .await
            .unwrap();
        let hosted = accepting.await.unwrap();
        assert_eq!(resumed.participant.incarnation, 2);
        host.disconnect(&hosted.participant).await;
        drop(resumed);
        drop(hosted);

        let accepting = {
            let host = host.clone();
            tokio::spawn(async move { host.accept(13).await })
        };
        assert!(Connected::connect(&old_resume, "replay".to_owned())
            .await
            .is_err());
        assert!(accepting.await.unwrap().is_err());
    }
}
