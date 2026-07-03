use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PkgBackendRequest {
    Probe(PkgProbeRequest),
    ResolveVersion(PkgResolveRequest),
    Install(PkgInstallRequest),
    Remove(PkgRemoveRequest),
    Doctor(PkgDoctorRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PkgBackendResponse {
    Probe(PkgProbeResponse),
    ResolveVersion(PkgResolveResponse),
    Install(PkgInstallResponse),
    Remove(PkgRemoveResponse),
    Doctor(PkgDoctorResponse),
    Progress(PkgProgress),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgProbeRequest {
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgProbeResponse {
    pub available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgResolveRequest {
    pub backend: String,
    pub package: String,
    pub reference: String,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgResolveResponse {
    pub version: String,
    pub url: String,
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgInstallRequest {
    pub backend: String,
    pub package: String,
    pub reference: String,
    pub version: String,
    pub staging_dir: String,
    pub bin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgInstallResponse {
    pub installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgRemoveRequest {
    pub backend: String,
    pub package: String,
    pub reference: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgRemoveResponse {
    pub removed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgDoctorRequest {
    pub backend: String,
    pub package: String,
    pub reference: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgDoctorResponse {
    pub ok: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgProgress {
    pub package: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkg_backend_contract_round_trips() {
        let request = PkgBackendRequest::Install(PkgInstallRequest {
            backend: "fixture".to_owned(),
            package: "demo".to_owned(),
            reference: "demo-ref".to_owned(),
            version: "1".to_owned(),
            staging_dir: "/tmp/demo".to_owned(),
            bin: "demo".to_owned(),
        });
        let bytes = super::super::codec::encode(&request).unwrap();
        let decoded: PkgBackendRequest = super::super::codec::decode(&bytes).unwrap();
        assert!(matches!(decoded, PkgBackendRequest::Install(_)));
    }
}
