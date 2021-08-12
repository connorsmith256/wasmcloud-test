//! simple test harness to load a capability provider and test it
//!
use anyhow::anyhow;
use async_trait::async_trait;
//use provider_archive::ProviderArchive;
use std::{
    convert::TryInto,
    fs,
    io::Write,
    path::{PathBuf},
};
use wasmbus_rpc::{
    core::{HealthCheckRequest, HealthCheckResponse, HostData, LinkDefinition, WasmCloudEntity},
    Message, RpcResult, SendOpts,
};
use toml::value::Value as TomlValue;

pub type SimpleValueMap = std::collections::HashMap<String, String>;
pub type TomlMap = std::collections::BTreeMap<String, toml::Value>;
pub type JsonMap = serde_json::Map<String, serde_json::Value>;

use futures::future::BoxFuture;
use termion::{color, style};
use tokio::sync::OnceCell;
use wasmbus_rpc::{provider::prelude::Context };

//type Result<T> = std::result::Result<T, anyhow::Error>;

const DEFAULT_NATS_URL: &str = "0.0.0.0:4222";

static ONCE: OnceCell<ProviderProcess> = OnceCell::const_new();
pub type TestFunc = fn() -> BoxFuture<'static, RpcResult<()>>;
//type TestVec = Vec<(&'static str, TestFunc)>;

/// Ways to specify linkdef values for the provider
#[allow(dead_code)]
pub enum LinkValues {
    /// the "classic" map of string keys and string values
    Values(SimpleValueMap),

    /// a json data structure that will be serialized and base64-encoded
    Json(JsonMap),

    /// data from a toml file that will be serialized and base64-encoded
    Toml(TomlMap),

    /// nothing - no values
    Empty,
}

impl TryInto<SimpleValueMap> for LinkValues {
    type Error = anyhow::Error;

    fn try_into(self) -> Result<SimpleValueMap, Self::Error> {
        let mut map = SimpleValueMap::default();
        match self {
            LinkValues::Values(s_map) => {
                map = s_map;
            }
            LinkValues::Json(json) => {
                let val = serde_json::to_string(&json)
                    .map_err(|e| anyhow!("link values serialization: {}", e.to_string()))?;
                let b64 = base64::encode_config(&val, base64::STANDARD_NO_PAD);
                map.insert("config_b64".to_string(), b64);
            }
            LinkValues::Toml(toml) => {
                let val = toml::to_string(&toml)
                    .map_err(|e| anyhow!("link values serialization: {}", e.to_string()))?;
                let b64 = base64::encode_config(&val, base64::STANDARD_NO_PAD);
                map.insert("config_b64".to_string(), b64);
            }
            LinkValues::Empty => {}
        }
        Ok(map)
    }
}

/// info needed to test a capability provider process. If this structure goes out of scope,
/// the provider will exit
pub struct ProviderProcess {
    pub file: std::fs::File,
    pub host_data: HostData,
    pub path: PathBuf,
    pub proc: std::process::Child,
    pub config: TomlMap,
    pub client: wasmbus_rpc::RpcClient,
}

impl ProviderProcess {
    /// generate the `origin` field for an Invocation. To the receiving provider,
    /// the origin field looks like an actor
    pub fn origin(&self) -> WasmCloudEntity {
        WasmCloudEntity {
            contract_id: "".to_string(),
            link_name: "".to_string(),
            public_key: self.host_data.host_id.to_string(),
        }
    }

    /// generate the `target` field for sending an Invocation to the provider
    pub fn target(&self) -> WasmCloudEntity {
        WasmCloudEntity {
            contract_id: "".to_string(),
            link_name: self.host_data.link_name.clone(),
            public_key: self.host_data.provider_key.clone(),
        }
    }

    /// link the test to the provider
    pub async fn link_to_test(&self, values: LinkValues) -> Result<(), anyhow::Error> {
        let topic = format!(
            "wasmbus.rpc.{}.{}.{}.linkdefs.put",
            &self.host_data.lattice_rpc_prefix,
            &self.host_data.provider_key,
            &self.host_data.link_name,
        );
        let origin = self.origin();
        let ld = LinkDefinition {
            actor_id: origin.public_key.clone(),
            contract_id: self
                .config
                .get("contract_id")
                .and_then(|v| v.as_str())
                .unwrap_or("wasmcloud:example")
                .to_string(),
            link_name: self.host_data.link_name.clone(),
            provider_id: self.host_data.provider_key.clone(),
            values: values.try_into()?,
        };
        //let bytes = wasmbus_rpc::serialize(&ld)?;
        let bytes = serde_json::to_vec(&ld)?;
        let _resp = self.client.publish(&topic, &bytes).await?;
        Ok(())
    }

    /// send a health check
    pub async fn health_check(&self) -> Result<(), anyhow::Error> {
        let topic = format!(
            "wasmbus.rpc.{}.{}.{}.health",
            &self.host_data.lattice_rpc_prefix,
            &self.host_data.provider_key,
            &self.host_data.link_name,
        );
        let resp: HealthCheckResponse = self
            .send_ctl_json(&topic, HealthCheckRequest::default())
            .await?;
        if !resp.healthy {
            return Err(anyhow!("provider returned unhealthy"));
        }
        Ok(())
    }

    /// send an rpc message to the provider
    pub async fn send_rpc(&self, message: Message<'_>) -> RpcResult<Vec<u8>> {
        self.client
            .send(self.origin(), self.target(), message)
            .await
    }

    /// send a control message to the provider:put link, get link, or shutdown
    pub async fn send_ctl_json<Arg, Resp>(&self, topic: &str, data: Arg) -> Result<Resp, anyhow::Error>
    where
        Arg: serde::Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        let bytes = serde_json::to_vec(&data)?;
        let resp_bytes = self.client.request(topic, &bytes).await?;
        let resp = serde_json::from_slice::<Resp>(&resp_bytes)?;
        Ok(resp)
    }

    /// Send shutdown signal to provider process
    pub async fn shutdown(&self) -> Result<(), anyhow::Error> {
        let shutdown_topic = format!(
            "wasmbus.rpc.{}.{}.{}.shutdown",
            &self.host_data.lattice_rpc_prefix, &self.host_data.provider_key, self.host_data.link_name
        );
        eprintln!("Sending shutdown to provider {}", &self.host_data.provider_key);
        self.client.publish(&shutdown_topic, b"").await?;
        Ok(())
    }
}

#[async_trait]
impl wasmbus_rpc::Transport for ProviderProcess {
    async fn send(
        &self,
        _ctx: &Context,
        message: Message<'_>,
        _opts: Option<SendOpts>,
    ) -> RpcResult<Vec<u8>> {
        self.send_rpc(message).await
    }
}

pub(crate) fn nats_url(config: &TomlMap) -> String {
    config
        .get("nats_url")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_NATS_URL)
        .to_string()
}

/// load toml configuration. looks for environment variable "PROVIDER_TEST_CONFIG",
/// otherwise loads defaults.
pub fn load_config() -> Result<TomlMap,anyhow::Error> {
    let path = if let Ok(path) = std::env::var("PROVIDER_TEST_CONFIG") {
        PathBuf::from(path)
    } else {PathBuf::from("./provider_test_config.toml")};
    let data = if !path.is_file() {
        Err(anyhow!(
                "Missing configuration file '{}'. Config file should be 'provider_test_config.toml' in the current directory, or a .toml file whose path is in the environment variable 'PROVIDER_TEST_CONFIG'", &path.display())
            )
    } else {
        fs::read_to_string(&path).map_err(|e| {
            anyhow!("failed reading config from {}: {}", &path.display(), e )
        })
    }?;
    let map = toml::from_str(&data).map_err(|e| anyhow!("parse error in configuration file loaded from {}: {}", &path.display(), e))?;
    Ok(map)
}

/// Starts a capability provider from its par file for testing.
/// Configuration file path should be in the environment variable PROVIDER_TEST_CONFIG
/// Par file path should be either in the environment variable PROVIDER_TEST_PAR
/// or in the config file as "par_file"
pub async fn start_provider_test(config: TomlMap) -> Result<ProviderProcess, anyhow::Error> {

    //CARGO_BIN_EXE_<name>

    /*
    let par_file = if let Ok(path) = std::env::var("PROVIDER_TEST_PAR") {
        let pb = PathBuf::from(&path);
        if !pb.is_file() {
            return Err(anyhow!(
                "no par file found at {} (from PROVIDER_TEST_PAR)",
                &path
            ));
        }
        pb
    } else {
        if let Some(path_val) = config.get("par_file") {
            let path = path_val.to_string();
            let pb = PathBuf::from(&path);
            if !pb.is_file() {
                return Err(anyhow!(
                    "no par file found at {} (from config 'par_file'). Update config or set the path in the environment variable PROVIDER_TEST_PAR",
                    &path
                ));
            }
            pb
        } else {
            return Err(anyhow!("Must specify par file in environment variable PROVIDER_TEST_PAR or in config file with the setting 'par_file'"));
        }
    };
    let (pubkey, exe_file, exe_path) = get_par_key_and_exe(&par_file)?;
    // make sure our handle is read-only
    let _ = exe_file.sync_all();
    let rd_file = fs::File::open(&exe_path)?;
    drop(exe_file);
    let exe_file = rd_file; // drop original and use read-only handle
     */
    let exe_path = match config.get("bin_path") {
        Some(TomlValue::String(name)) =>  PathBuf::from(name),
        _ => return Err(anyhow!("Must specifiy binary path in 'bin_path' in config file"))
    };
    let pubkey = format!("_PubKey_{}", &exe_path.display());
    let exe_file = fs::File::open(&exe_path)?;

    // set logging level for capability provider with the "RUST_LOG" environment variable,
    // default level is "info"
    let log_level = match config.get("rust_log") {
        Some(TomlValue::String(level)) if level.parse::<log::Level>().is_ok() => {
                level.to_string()
        }
        None => "info".to_string(),
        Some(x) => {
            eprintln!("invalid 'rust_log' setting '{}', using 'info'", x.to_string());
            "info".to_string()
        }
    };
    // set RUST_BACKTRACE, if requested
    // default is disabled
    let enable_backtrace = match config.get("rust_backtrace") {
        Some(TomlValue::String(sval)) if sval.as_str() == "1" => "1",
        Some(TomlValue::Integer(num)) if *num > 0 => "1",
        Some(TomlValue::Boolean(bval)) if *bval => "1",
        _ => "0",
    };

    let host_data = HostData {
        host_id: "_TEST_".to_string(),
        lattice_rpc_prefix: config
            .get("lattice_rpc_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string(),
        lattice_rpc_url: nats_url(&config),
        link_name: config
            .get("link_name")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string(),
        provider_key: pubkey,
        ..Default::default()
    };
    let buf = serde_json::to_vec(&host_data)?;
    let mut encoded = base64::encode_config(&buf, base64::STANDARD_NO_PAD);
    encoded.push_str("\r\n");

    // provider's stdout is piped through our stdout
    let mut child_proc = std::process::Command::new(&exe_path)
        .stdin(std::process::Stdio::piped())
        .env("RUST_LOG", &log_level)
        .env("RUST_BACKTRACE", enable_backtrace )
        .spawn()
        .map_err(|e| anyhow!("launching provider bin at {}: {}", &exe_path.display(), e))?;

    let mut stdin = child_proc
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open child stdin"))?;

    // come back and check on whether we need another process. don't think so
    stdin.write_all(encoded.as_bytes())?;

    // Connect to nats
    let nc = ratsio::NatsClient::new(host_data.nats_options())
        .await
        .map_err(|e| {
            anyhow!(
                "nats connection to {} failed: {}",
                &host_data.lattice_rpc_url,
                e.to_string()
            )
        })?;
    let keys = wascap::prelude::KeyPair::new_user();
    let client = wasmbus_rpc::RpcClient::new(nc, &host_data.lattice_rpc_prefix, keys);

    Ok(ProviderProcess {
        file: exe_file,
        path: exe_path,
        proc: child_proc,
        host_data,
        config,
        client,
    })
}

/*
/// get the provider public key and native executable
fn get_par_key_and_exe(path: &Path) -> Result<(String, fs::File, PathBuf)> {
    // read archive file into memory and interpret as par file
    let bytes = fs::read(path)
        .map_err(|e| anyhow!("failed to read archive at {}: {}", &path.display(), e))?;
    let archive = ProviderArchive::try_load(&bytes)
        .map_err(|e| anyhow!("invalid par format for {}: {}", &path.display(), e))?;

    let pub_key = archive.claims().ok_or_else(|| {
        anyhow!(
            "par file {} has invalid format: no public key",
            path.display()
        )
    })?;
    let (file, path) = extract_target_exe(&archive)?;
    Ok((pub_key.subject, file, path))
}
 */

/*
/// get target tuple, e.g., "x86_64-linux"
fn native_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}
 */

/*
/// save native binary from par into temp file
fn extract_target_exe(par: &ProviderArchive) -> Result<(fs::File, PathBuf)> {
    let target = native_target();
    let bytes = par
        .target_bytes(&target)
        .ok_or_else(|| anyhow!("no target found for platform {} in par file", &target))?;
    let (file, path) = save_to_tempfile(&bytes)
        .map_err(|e| anyhow!("error extracting bin file from par: {}", e))?;
    set_executable_mode(&path)?;
    Ok((file, path))
}
 */

/*
/// make the extracted file executable
// #[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(unix)]
fn set_executable_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    eprintln!("setting file permission");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
 */

/*
// #[cfg(target_family = "windows")]
#[cfg(not(unix))]
fn set_executable_mode(file: fs::File) -> Result<()> {
    todo!()
}

 */
/*
/// create temporary file and save bytes to it,
/// returning file handle and its path
fn save_to_tempfile(bytes: &[u8]) -> Result<(fs::File, PathBuf)> {
    use tempfile::NamedTempFile;

    // Create a file inside of `std::env::temp_dir()`.
    let mut temp = NamedTempFile::new()?;

    let file = temp.as_file_mut();
    file.write_all(bytes)?;
    Ok(temp.keep()?)
}
 */


/// Execute all tests. In the current implementation,
/// all tests are run in sequentially, and always in the same order.
// A future version of this should take a parameter for a scheduling strategy,
// which could permit options such as
// enum RunStrategy{
//   Deterministic,
//   Random,
//   Parallel(u16),  // num_threads
// }
pub async fn run_tests(tests: Vec<(&'static str, TestFunc)>) -> std::result::Result<(usize,usize),Box<dyn std::error::Error>>{
    let mut passed = 0usize;
    let total = tests.len();
    let handle = tokio::runtime::Handle::current();
    for t in tests.into_iter() {
        match handle.spawn((&t.1)()).await? {
            Ok(()) => {
                println!("{} Pass {}: {}",
                         color::Fg(color::Green), style::Reset, t.0);
                passed += 1;
            }
            Err(e) => {
                println!(
                    "{} Fail {}: {}  {}",
                    color::Fg(color::Red),
                    style::Reset,
                    t.0,
                    e.to_string()
                );
            }
        };
    }

    let provider = test_provider().await;
    let _ = provider.shutdown().await; // send shutdown message

    let status_color = if passed == total {
        color::Fg(color::Green).to_string()
    } else {
        color::Fg(color::Red).to_string()
    };
    println!("Test results: {}{}/{} Passed{}",
        status_color, passed, total, style::Reset);

    Ok((passed,total))
}

pub async fn test_provider() -> &'static ProviderProcess {
    ONCE.get_or_init(|| async {
        match load_provider().await {
            Ok(p) => p,
            Err(e) => {
                panic!("failed to load provider: {}", e.to_string());
            }
        }
    })
        .await
}

pub async fn load_provider() -> Result<ProviderProcess, Box<dyn std::error::Error>> {
    let conf = load_config()?;
    let prov = start_provider_test(conf).await?;

    // give it time to startup
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // set link params so we can send rpc
    prov.link_to_test(LinkValues::Empty).await?;

    Ok(prov)
}