use serde::{Deserialize, Serialize};
use std::io::Cursor;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DeployResult {
    pub valid: Result<String, String>,
    #[serde(default)]
    pub vols: Vec<ContainerVolume>,
    #[serde(default)]
    pub start_mode: StartMode,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ContainerVolume {
    pub name: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum StartMode {
    Empty,
    Blocking,
}

impl Default for StartMode {
    fn default() -> Self {
        StartMode::Empty
    }
}

impl DeployResult {
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> anyhow::Result<DeployResult> {
        let b = bytes.as_ref();
        if b.is_empty() {
            return Ok(DeployResult {
                valid: Ok(Default::default()),
                vols: Default::default(),
                start_mode: Default::default(),
            });
        }
        if b[0] != b'{' {
            anyhow::bail!("invliad deploy response")
        }
        Ok(serde_json::from_reader(Cursor::new(b))?)
    }
}

#[cfg(test)]
mod test {
    use super::{ContainerVolume, DeployResult};
    use std::io;

    fn parse_bytes<T: AsRef<[u8]>>(b: T) -> DeployResult {
        let result = DeployResult::from_bytes(b).unwrap();
        eprintln!("result={:?}", result);
        result
    }

    #[test]
    fn test_wasi_deploy() {
        parse_bytes(
            r#"{
            "valid": {"Ok": "success"}
        }"#,
        );
        parse_bytes(
            r#"{
            "valid": {"Err": "bad image format"}
        }"#,
        );
        parse_bytes(
            r#"{
            "valid": {"Ok": "success"},
            "vols": [
                {"name": "vol-9a0c1c4a", "path": "/in"},
                {"name": "vol-a68672e0", "path": "/out"}
            ]
        }"#,
        );
    }
}
