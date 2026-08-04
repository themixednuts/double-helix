use crate::{
    Credential, ParticipantId, ParticipantInfo, Request, Role, SecretToken, SessionId, MAX_INVITES,
    MAX_PARTICIPANTS, MAX_PARTICIPANT_NAME_BYTES, PROTOCOL_VERSION,
};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, time::Duration};

pub(crate) const RESUME_TTL: Duration = Duration::from_secs(60 * 60 * 24);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub info: ParticipantInfo,
    pub connected: bool,
}

#[derive(Debug, Clone)]
pub struct Invitation {
    pub session: SessionId,
    pub token: SecretToken,
    pub role: Role,
    pub expires_unix_secs: u64,
}

#[derive(Debug, Clone)]
pub struct Authenticated {
    pub participant: ParticipantInfo,
    pub resume: SecretToken,
    pub resume_expires_unix_secs: u64,
}

struct Grant {
    role: Role,
    expires_unix_secs: u64,
}

struct ResumeGrant {
    participant: ParticipantId,
    expires_unix_secs: u64,
}

pub struct HostSession {
    id: SessionId,
    owner: ParticipantId,
    participants: HashMap<ParticipantId, Participant>,
    invites: HashMap<[u8; 32], Grant>,
    resumes: HashMap<[u8; 32], ResumeGrant>,
}

impl HostSession {
    pub fn new(owner_name: impl Into<String>) -> Result<Self, AuthError> {
        let owner_name = owner_name.into();
        validate_name(&owner_name)?;
        let id = SessionId(random_bytes()?);
        let owner = ParticipantId(random_bytes()?);
        let participants = HashMap::from([(
            owner,
            Participant {
                info: ParticipantInfo {
                    id: owner,
                    name: owner_name,
                    role: Role::Owner,
                    incarnation: 1,
                },
                connected: true,
            },
        )]);
        Ok(Self {
            id,
            owner,
            participants,
            invites: HashMap::new(),
            resumes: HashMap::new(),
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn owner(&self) -> ParticipantId {
        self.owner
    }

    pub fn participant(&self, id: ParticipantId) -> Option<&Participant> {
        self.participants.get(&id)
    }

    pub fn is_disconnected(&self, participant: &ParticipantInfo) -> bool {
        self.participants
            .get(&participant.id)
            .is_some_and(|current| {
                current.info.incarnation == participant.incarnation && !current.connected
            })
    }

    pub fn participants(&self) -> impl Iterator<Item = &Participant> {
        self.participants.values()
    }

    pub fn participant_infos(&self) -> Vec<ParticipantInfo> {
        self.participants
            .values()
            .filter(|participant| participant.connected)
            .map(|participant| participant.info.clone())
            .collect()
    }

    pub fn issue_owner_resume(&mut self, now_unix_secs: u64) -> Result<Authenticated, AuthError> {
        let owner = self
            .participants
            .get(&self.owner)
            .ok_or(AuthError::UnknownParticipant)?
            .info
            .clone();
        self.issue_resume(owner, now_unix_secs)
    }

    pub fn invite(
        &mut self,
        actor: ParticipantId,
        role: Role,
        expires_unix_secs: u64,
        now_unix_secs: u64,
    ) -> Result<Invitation, AuthError> {
        self.authorize(actor, Role::Owner)?;
        if role == Role::Owner {
            return Err(AuthError::OwnerRoleReserved);
        }
        if expires_unix_secs <= now_unix_secs {
            return Err(AuthError::Expired);
        }
        if self.invites.len() >= MAX_INVITES {
            return Err(AuthError::InviteLimit);
        }
        let token = SecretToken(random_bytes()?);
        self.invites.insert(
            token_hash(&token),
            Grant {
                role,
                expires_unix_secs,
            },
        );
        Ok(Invitation {
            session: self.id,
            token,
            role,
            expires_unix_secs,
        })
    }

    pub fn authenticate(
        &mut self,
        protocol: u16,
        session: SessionId,
        credential: Credential,
        name: String,
        now_unix_secs: u64,
    ) -> Result<Authenticated, AuthError> {
        if protocol != PROTOCOL_VERSION {
            return Err(AuthError::ProtocolMismatch {
                client: protocol,
                host: PROTOCOL_VERSION,
            });
        }
        if session != self.id {
            return Err(AuthError::WrongSession);
        }
        validate_name(&name)?;
        match credential {
            Credential::Invite(token) => self.accept_invite(token, name, now_unix_secs),
            Credential::Resume(token) => self.accept_resume(token, name, now_unix_secs),
        }
    }

    pub fn authorize(&self, actor: ParticipantId, required: Role) -> Result<(), AuthError> {
        let participant = self
            .participants
            .get(&actor)
            .ok_or(AuthError::UnknownParticipant)?;
        if !participant.connected {
            return Err(AuthError::Disconnected);
        }
        if !participant.info.role.allows(required) {
            return Err(AuthError::Forbidden {
                actual: participant.info.role,
                required,
            });
        }
        Ok(())
    }

    pub fn authorize_request(
        &self,
        actor: ParticipantId,
        incarnation: u64,
        request: &Request,
    ) -> Result<(), AuthError> {
        if self
            .participants
            .get(&actor)
            .is_some_and(|participant| participant.info.incarnation != incarnation)
        {
            return Err(AuthError::StaleConnection);
        }
        self.authorize(actor, request.required_role())?;
        if matches!(
            request,
            Request::PublishPresence(presence) if presence.participant != actor
        ) {
            return Err(AuthError::IdentityMismatch);
        }
        Ok(())
    }

    pub fn set_role(
        &mut self,
        actor: ParticipantId,
        participant: ParticipantId,
        role: Role,
    ) -> Result<(), AuthError> {
        self.authorize(actor, Role::Owner)?;
        if participant == self.owner || role == Role::Owner {
            return Err(AuthError::OwnerRoleReserved);
        }
        self.participants
            .get_mut(&participant)
            .ok_or(AuthError::UnknownParticipant)?
            .info
            .role = role;
        Ok(())
    }

    pub fn disconnect(&mut self, participant: ParticipantId, incarnation: u64) -> bool {
        if let Some(participant) = self.participants.get_mut(&participant) {
            if participant.info.incarnation == incarnation {
                participant.connected = false;
                return true;
            }
        }
        false
    }

    pub fn remove(
        &mut self,
        actor: ParticipantId,
        participant: ParticipantId,
    ) -> Result<Participant, AuthError> {
        self.authorize(actor, Role::Owner)?;
        if participant == self.owner {
            return Err(AuthError::OwnerRoleReserved);
        }
        self.resumes
            .retain(|_, grant| grant.participant != participant);
        self.participants
            .remove(&participant)
            .ok_or(AuthError::UnknownParticipant)
    }

    fn accept_invite(
        &mut self,
        token: SecretToken,
        name: String,
        now_unix_secs: u64,
    ) -> Result<Authenticated, AuthError> {
        if self.participants.len() >= MAX_PARTICIPANTS {
            return Err(AuthError::ParticipantLimit);
        }
        let grant = self
            .invites
            .remove(&token_hash(&token))
            .ok_or(AuthError::InvalidCredential)?;
        if grant.expires_unix_secs <= now_unix_secs {
            return Err(AuthError::Expired);
        }
        let id = ParticipantId(random_bytes()?);
        let info = ParticipantInfo {
            id,
            name,
            role: grant.role,
            incarnation: 1,
        };
        self.participants.insert(
            id,
            Participant {
                info: info.clone(),
                connected: true,
            },
        );
        self.issue_resume(info, now_unix_secs)
    }

    fn accept_resume(
        &mut self,
        token: SecretToken,
        name: String,
        now_unix_secs: u64,
    ) -> Result<Authenticated, AuthError> {
        let grant = self
            .resumes
            .remove(&token_hash(&token))
            .ok_or(AuthError::InvalidCredential)?;
        if grant.expires_unix_secs <= now_unix_secs {
            return Err(AuthError::Expired);
        }
        let participant = self
            .participants
            .get_mut(&grant.participant)
            .ok_or(AuthError::UnknownParticipant)?;
        participant.info.name = name;
        participant.info.incarnation = participant.info.incarnation.saturating_add(1);
        participant.connected = true;
        let info = participant.info.clone();
        self.issue_resume(info, now_unix_secs)
    }

    fn issue_resume(
        &mut self,
        participant: ParticipantInfo,
        now_unix_secs: u64,
    ) -> Result<Authenticated, AuthError> {
        let resume = SecretToken(random_bytes()?);
        let resume_expires_unix_secs = now_unix_secs.saturating_add(RESUME_TTL.as_secs());
        self.resumes.insert(
            token_hash(&resume),
            ResumeGrant {
                participant: participant.id,
                expires_unix_secs: resume_expires_unix_secs,
            },
        );
        Ok(Authenticated {
            participant,
            resume,
            resume_expires_unix_secs,
        })
    }
}

fn validate_name(name: &str) -> Result<(), AuthError> {
    if name.is_empty()
        || name.len() > MAX_PARTICIPANT_NAME_BYTES
        || name.chars().any(char::is_control)
    {
        Err(AuthError::InvalidName)
    } else {
        Ok(())
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], AuthError> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|error| AuthError::Entropy(error.to_string()))?;
    Ok(bytes)
}

fn token_hash(token: &SecretToken) -> [u8; 32] {
    Sha256::digest(token.0).into()
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("collaboration protocol mismatch: client {client}, host {host}")]
    ProtocolMismatch { client: u16, host: u16 },
    #[error("credential belongs to another collaboration session")]
    WrongSession,
    #[error("collaboration credential is invalid or has already been used")]
    InvalidCredential,
    #[error("collaboration credential has expired")]
    Expired,
    #[error("participant name is empty, too long, or contains a control character")]
    InvalidName,
    #[error("collaboration participant is unknown")]
    UnknownParticipant,
    #[error("collaboration participant is disconnected")]
    Disconnected,
    #[error("collaboration connection was replaced by a newer session incarnation")]
    StaleConnection,
    #[error("collaboration request participant does not match its authenticated sender")]
    IdentityMismatch,
    #[error("collaboration action requires {required:?}, participant has {actual:?}")]
    Forbidden { actual: Role, required: Role },
    #[error("the owner role cannot be delegated, changed, or removed")]
    OwnerRoleReserved,
    #[error("collaboration participant limit reached")]
    ParticipantLimit,
    #[error("collaboration invite limit reached")]
    InviteLimit,
    #[error("operating-system entropy is unavailable: {0}")]
    Entropy(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BufferId, Presence, TextAnchor, ViewId};
    use serde_bytes::ByteBuf;

    #[test]
    fn invites_are_single_use_and_resume_tokens_rotate() {
        let mut session = HostSession::new("owner").unwrap();
        let invite = session
            .invite(session.owner(), Role::Write, 100, 10)
            .unwrap();
        let joined = session
            .authenticate(
                PROTOCOL_VERSION,
                session.id(),
                Credential::Invite(invite.token.clone()),
                "guest".to_owned(),
                11,
            )
            .unwrap();
        assert!(matches!(
            session.authenticate(
                PROTOCOL_VERSION,
                session.id(),
                Credential::Invite(invite.token),
                "replay".to_owned(),
                12,
            ),
            Err(AuthError::InvalidCredential)
        ));

        session.disconnect(joined.participant.id, joined.participant.incarnation);
        assert!(session.is_disconnected(&joined.participant));
        assert!(!session
            .participant_infos()
            .iter()
            .any(|participant| participant.id == joined.participant.id));
        let resumed = session
            .authenticate(
                PROTOCOL_VERSION,
                session.id(),
                Credential::Resume(joined.resume.clone()),
                "guest".to_owned(),
                13,
            )
            .unwrap();
        assert_eq!(resumed.participant.id, joined.participant.id);
        assert!(!session.is_disconnected(&joined.participant));
        assert!(!session.is_disconnected(&resumed.participant));
        assert!(matches!(
            session.authenticate(
                PROTOCOL_VERSION,
                session.id(),
                Credential::Resume(joined.resume),
                "replay".to_owned(),
                14,
            ),
            Err(AuthError::InvalidCredential)
        ));
    }

    #[test]
    fn every_request_is_authorized_at_the_host() {
        let mut session = HostSession::new("owner").unwrap();
        let invite = session
            .invite(session.owner(), Role::Read, 100, 10)
            .unwrap();
        let guest = session
            .authenticate(
                PROTOCOL_VERSION,
                session.id(),
                Credential::Invite(invite.token),
                "reader".to_owned(),
                11,
            )
            .unwrap();
        let presence = Request::PublishPresence(Presence {
            participant: guest.participant.id,
            buffer: BufferId(1),
            cursor: Some(TextAnchor(ByteBuf::from(vec![1]))),
            selection: None,
            viewport: None,
            active_view: Some(ViewId([2; 16])),
        });
        assert!(session
            .authorize_request(
                guest.participant.id,
                guest.participant.incarnation,
                &presence,
            )
            .is_ok());
        assert!(matches!(
            session.authorize_request(
                guest.participant.id,
                guest.participant.incarnation,
                &Request::SaveBuffer {
                    buffer: BufferId(1),
                    overwrite: false,
                }
            ),
            Err(AuthError::Forbidden { .. })
        ));
    }

    #[test]
    fn owner_resume_authenticates_the_existing_owner_identity() {
        let mut session = HostSession::new("owner").unwrap();
        let owner = session.owner();
        let issued = session.issue_owner_resume(10).unwrap();
        let authenticated = session
            .authenticate(
                PROTOCOL_VERSION,
                session.id(),
                Credential::Resume(issued.resume),
                "owner".to_owned(),
                11,
            )
            .unwrap();

        assert_eq!(authenticated.participant.id, owner);
        assert_eq!(authenticated.participant.role, Role::Owner);
    }
}
