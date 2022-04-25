use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[macro_use]
extern crate rocket;
use bollard::image::{ListImagesOptions, RemoveImageOptions, TagImageOptions};
use rocket::fairing::AdHoc;
use rocket::form::{self, Form};
use rocket::http::hyper::Body;
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::tokio::fs;
use rocket::tokio::io::AsyncWriteExt;
use rocket::tokio::time::error::Elapsed;
use rocket::tokio::time::{timeout_at, Instant};
use rocket::{tokio, State};

#[macro_use(defer)]
extern crate scopeguard;
use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, LogOutput, LogsOptions,
    RemoveContainerOptions,
};
use bollard::{image::BuildImageOptions, Docker};
use lazy_static::lazy_static;
use regex::Regex;

use futures_util::stream::StreamExt;
use git2::Repository;
use tar::Builder;

#[derive(Deserialize, Debug)]
struct RunnerConfig {
    execution_root: String,
    compilation_root: String,
    docker_image_prefix: String,
    docker_exec_prefix: String,
    exec_workdir_in_docker: String,
    user_uid_gid: String,
    #[serde(default = "five_minutes")]
    max_timeout: u64,
}

fn five_minutes() -> u64 {
    5 * 60
}

type DemoID = String;

fn validate_demoid<'v>(s: &str) -> form::Result<'v, ()> {
    lazy_static! {
        static ref RE: Regex = Regex::new(r"^\w+$").unwrap();
    }
    if !RE.is_match(s) {
        return Err(rocket::form::Error::validation("invalid demo_id").into());
    }
    Ok(())
}

type RunKey = String;

fn validate_runkey<'v>(s: &str) -> form::Result<'v, ()> {
    lazy_static! {
        static ref RE: Regex = Regex::new(r"^\w+$").unwrap();
    }
    if !RE.is_match(s) {
        return Err(rocket::form::Error::validation("invalid key").into());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
enum ParamValue {
    Bool(bool),
    PosInt(u64),
    NegInt(i64),
    Float(f64),
    String(String),
}
impl std::fmt::Display for ParamValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamValue::Bool(v) => write!(f, "{}", v),
            ParamValue::PosInt(v) => write!(f, "{}", v),
            ParamValue::NegInt(v) => write!(f, "{}", v),
            ParamValue::Float(v) => write!(f, "{}", v),
            ParamValue::String(v) => write!(f, "{}", v),
        }
    }
}

type RunParams = HashMap<String, ParamValue>;

trait ToEnvVec {
    fn is_valid_param_name(name: &str) -> bool {
        const INVALID_NAMES: &[&str] = &[
            "HOSTNAME",
            "PATH",
            "HOME",
            "LANG",
            "TERM",
            "LD_LIBRARY_PATH",
            "LD_PRELOAD",
            "PYTHONPATH",
            "PERLLIB",
            "RUBYLIB",
            "CLASSPATH",
            "NODE_PATH",
            "IPOL_DEMOID",
            "IPOL_KEY",
        ];
        !INVALID_NAMES.contains(&name)
    }

    fn to_env_vec(&self, demo_id: &DemoID, key: &RunKey) -> Vec<String>;
}
impl ToEnvVec for RunParams {
    fn to_env_vec(&self, demo_id: &DemoID, key: &RunKey) -> Vec<String> {
        let mut env = vec![
            format!("IPOL_DEMOID={}", demo_id),
            format!("IPOL_KEY={}", key),
        ];
        for (name, value) in self
            .iter()
            .filter(|(name, _)| Self::is_valid_param_name(name))
        {
            // TODO: issue if name contains a =
            env.push(format!("{}={}", name, value));
        }
        env
    }
}

type DDLRun = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DDLBuild {
    url: String,
    rev: String,
    dockerfile: String,
}

#[derive(Debug, Serialize)]
struct PingResponse {
    status: String,
    ping: String,
}

#[get("/ping")]
fn ping() -> Json<PingResponse> {
    Json(PingResponse {
        status: "OK".into(),
        ping: "pong".into(),
    })
}

#[derive(Debug, Serialize)]
struct ShutdownResponse {
    status: String,
}

#[get("/shutdown")]
fn shutdown(shutdown: rocket::Shutdown) -> Json<ShutdownResponse> {
    shutdown.notify();
    Json(ShutdownResponse {
        status: "OK".into(),
    })
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Workload {
    status: String,
    workload: f32,
}

#[get("/get_workload")]
fn get_workload() -> Json<Workload> {
    Json(Workload {
        status: "OK".into(),
        workload: 1.0,
    })
}

#[derive(Debug, FromForm)]
struct EnsureCompilationRequest {
    #[field(validate=validate_demoid())]
    demo_id: DemoID,
    ddl_build: Json<DDLBuild>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct EnsureCompilationResponse {
    status: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    buildlog: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum CompilationError {
    #[error("Compilation error")]
    BuildError(String),
    #[error("{0}")]
    IO(#[from] std::io::Error),
    #[error("{0}")]
    Docker(#[from] bollard::errors::Error),
    #[error("{0}")]
    Git(#[from] git2::Error),
    #[error("Couldn't find dockerfile: {0}")]
    MissingDockerfile(String),
}

fn url_of_git_repository(srcdir: &Path) -> Option<String> {
    let repo = Repository::open(srcdir).ok()?;
    let remote = repo.find_remote("origin").ok()?;
    let url = remote.url()?;
    Some(String::from(url))
}

// from https://docs.rs/git2/0.14.2/src/git2/repo.rs.html#328
fn update_submodules(repo: &git2::Repository) -> Result<(), git2::Error> {
    fn add_subrepos(repo: &Repository, list: &mut Vec<Repository>) -> Result<(), git2::Error> {
        for mut subm in repo.submodules()? {
            subm.update(true, None)?;
            list.push(subm.open()?);
        }
        Ok(())
    }

    let mut repos = Vec::new();
    add_subrepos(repo, &mut repos)?;
    while let Some(repo) = repos.pop() {
        add_subrepos(&repo, &mut repos)?;
    }
    Ok(())
}

fn prepare_git(path: &Path, url: &str, rev: &str) -> Result<String, CompilationError> {
    // NOTE: libgit2 does not support shallow fetches yet
    if let Some(current_url) = url_of_git_repository(path) {
        if current_url != url {
            std::fs::remove_dir_all(path)?;
        }
    } else if path.exists() {
        std::fs::remove_dir_all(path)?;
    }

    let get_fetch_options = || {
        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.credentials(|_url, username_from_url, _allowed_types| {
            git2::Cred::ssh_key(
                username_from_url.unwrap(),
                None,
                Path::new(&format!("{}/.ssh/id_rsa", std::env::var("HOME").unwrap())),
                None,
            )
        });
        let mut fo = git2::FetchOptions::new();
        fo.remote_callbacks(callbacks);
        fo
    };

    let repo = if !path.exists() {
        let fo = get_fetch_options();
        let mut builder = git2::build::RepoBuilder::new();
        builder.fetch_options(fo);
        builder.clone(url, path)?
    } else {
        Repository::open(path)?
    };

    {
        let mut fo = get_fetch_options();
        let mut remote = repo.find_remote("origin")?;
        remote.fetch(&["master"], Some(&mut fo), None)?;
    }

    // TODO: support "master" as rev instead of "origin/master"
    let object = repo.revparse_single(rev)?;
    let commit = object.id().to_string();
    repo.checkout_tree(&object, None)?;
    repo.set_head_detached(object.id())?;

    update_submodules(&repo)?;

    Ok(commit)
}

async fn ensure_compilation_inner(
    req: Form<EnsureCompilationRequest>,
    config: &State<RunnerConfig>,
) -> Result<(), CompilationError> {
    dbg!(&req);

    let compilation_path = PathBuf::from(&config.compilation_root).join(&req.demo_id);
    let srcdir = PathBuf::from(&compilation_path).join("src");
    let logfile = PathBuf::from(&compilation_path).join("build.log");
    fs::create_dir_all(&compilation_path).await?;
    let mut buildlog = fs::File::create(logfile).await?;

    let git_rev = {
        let srcdir = srcdir.clone();
        let ddl_build = req.ddl_build.clone();
        tokio::task::spawn_blocking(move || prepare_git(&srcdir, &ddl_build.url, &ddl_build.rev))
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Interrupted, e))??
    };

    if !PathBuf::from(&srcdir)
        .join(&req.ddl_build.dockerfile)
        .exists()
    {
        return Err(CompilationError::MissingDockerfile(
            req.ddl_build.dockerfile.clone(),
        ));
    }

    let docker = Docker::connect_with_socket_defaults()?;
    let image_name = format!("{}{}", config.docker_image_prefix, req.demo_id);
    let image_name_with_tag = format!("{}:{}", image_name, git_rev);

    let filters: HashMap<&str, Vec<&str>> =
        HashMap::from([("reference", vec![image_name.as_ref()])]);
    let current_images = docker
        .list_images(Some(ListImagesOptions {
            filters,
            ..Default::default()
        }))
        .await?;

    if current_images
        .iter()
        .any(|img| img.repo_tags.iter().any(|t| t == &image_name_with_tag))
    {
        buildlog
            .write(format!("(docker image already exists for commit {})", git_rev).as_bytes())
            .await?;
        return Ok(());
    }

    let build_image_options = BuildImageOptions {
        dockerfile: req.ddl_build.dockerfile.clone(),
        t: image_name_with_tag.clone(),
        q: true,
        rm: true,
        ..Default::default()
    };

    let vec = {
        let srcdir = srcdir.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CompilationError> {
            let mut ar = Builder::new(Vec::new());
            // TODO: exclude .git
            ar.append_dir_all(".", srcdir)?;
            Ok(ar.into_inner()?)
        })
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Interrupted, e))??
    };
    let tar = Body::from(vec);

    let mut image_build_stream = docker.build_image(build_image_options, None, Some(tar));
    let mut buildlogbuf = String::new();
    let mut errored = false;
    while let Some(msg) = image_build_stream.next().await {
        let info = msg?;
        if let Some(stream) = info.stream {
            buildlog.write_all(stream.as_bytes()).await?;
            buildlogbuf.push_str(&stream);
        }
        if let Some(err) = info.error {
            buildlog.write_all(err.as_bytes()).await?;
            buildlogbuf.push_str(&err);
            errored = true;
        }
    }

    if errored {
        return Err(CompilationError::BuildError(buildlogbuf));
    }

    for image in current_images {
        docker
            .remove_image(
                &image.id,
                Some(RemoveImageOptions {
                    force: true,
                    ..Default::default()
                }),
                None,
            )
            .await?;
    }

    docker
        .tag_image(
            &image_name_with_tag,
            Some(TagImageOptions {
                repo: image_name_with_tag.as_ref(),
                tag: "latest",
            }),
        )
        .await?;

    Ok(())
}

#[post("/ensure_compilation", data = "<req>")]
async fn ensure_compilation(
    req: Form<EnsureCompilationRequest>,
    config: &State<RunnerConfig>,
) -> Json<EnsureCompilationResponse> {
    let response = match ensure_compilation_inner(req, config).await {
        Ok(()) => EnsureCompilationResponse {
            status: "OK".into(),
            message: String::new(),
            buildlog: None,
        },
        Err(err) => match err {
            CompilationError::BuildError(ref buildlog) => EnsureCompilationResponse {
                status: "KO".into(),
                message: err.to_string(),
                buildlog: Some(buildlog.clone()),
            },
            _ => EnsureCompilationResponse {
                status: "KO".into(),
                message: err.to_string(),
                buildlog: None,
            },
        },
    };
    Json(response)
}

#[derive(Debug, FromForm)]
struct ExecAndWaitRequest {
    #[field(validate=validate_demoid())]
    demo_id: DemoID,
    #[field(validate=validate_runkey())]
    key: RunKey,
    params: Json<RunParams>,
    ddl_run: Json<DDLRun>,
    timeout: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AlgoInfo {
    error_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_time: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ExecAndWaitResponse {
    key: RunKey,
    params: RunParams,
    status: String,
    error: String,
    algo_info: AlgoInfo,
}

#[derive(Debug, thiserror::Error)]
enum ExecError {
    #[error("Non-zero exit code ({0}): {1}")]
    NonZeroExitCode(i64, String),
    #[error("{0}")]
    IO(#[from] std::io::Error),
    #[error("{0}")]
    Docker(#[from] bollard::errors::Error),
    #[error("IPOLTimeoutError: Execution timeout")]
    Timeout(#[from] Elapsed),
}

async fn exec_and_wait_inner(
    req: &ExecAndWaitRequest,
    config: &RunnerConfig,
) -> Result<Duration, ExecError> {
    dbg!(&req);

    let outdir = PathBuf::from(&config.execution_root)
        .join(&req.demo_id)
        .join(&req.key);
    fs::create_dir_all(&outdir).await?;
    let outdir = fs::canonicalize(outdir).await?;

    let image_name = format!("{}{}:latest", config.docker_image_prefix, req.demo_id);
    let exec_mountpoint = &config.exec_workdir_in_docker;

    let mut stderr = fs::File::create(outdir.join("stderr.txt")).await?;
    let mut stdout = fs::File::create(outdir.join("stdout.txt")).await?;

    let host_config = bollard::models::HostConfig {
        binds: Some(vec![format!(
            "{}:{}",
            outdir.clone().into_os_string().into_string().unwrap(),
            exec_mountpoint,
        )]),
        ..Default::default()
    };

    let name = format!("{}{}-{}", config.docker_exec_prefix, req.demo_id, req.key);
    let options = Some(CreateContainerOptions {
        name: name.as_str(),
    });

    let env = req.params.clone().to_env_vec(&req.demo_id, &req.key);
    let env = env.iter().map(|s| s as &str).collect();
    let container_config = Config {
        image: Some(image_name.as_str()),
        user: Some(&config.user_uid_gid),
        cmd: Some(vec!["/bin/bash", "-c", req.ddl_run.as_str()]),
        env: Some(env),
        working_dir: Some(exec_mountpoint),
        host_config: Some(host_config),
        // GPU device: https://github.com/fussybeaver/bollard/issues/216#issuecomment-1067637628
        ..Default::default()
    };

    let docker = Docker::connect_with_socket_defaults()?;
    let id = docker.create_container(options, container_config).await?.id;
    dbg!(&id);

    defer! {
        let docker = docker.clone();
        let name = name.clone();
        rocket::tokio::spawn(async move {
            let options = Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            });
            if let Err(e) = docker.remove_container(&name, options).await {
                error!("{}", e);
            }
        });
    }

    docker.start_container::<String>(&id, None).await?;

    let mut output = String::new();
    let max_timeout = config.max_timeout;
    let timeout = req.timeout.map_or(max_timeout, |v| max_timeout.min(v));
    let deadline = Instant::now() + Duration::from_secs(timeout);
    timeout_at(deadline, async {
        let options = Some(LogsOptions::<String> {
            follow: true,
            stdout: true,
            stderr: true,
            ..Default::default()
        });
        let mut logs = docker.logs(&id, options);
        while let Some(msg) = logs.next().await {
            match msg {
                Ok(LogOutput::StdOut { message }) => {
                    println!("stdout: {message:#?}");
                    stdout.write_all(&message).await?;
                    output.push_str(&String::from_utf8_lossy(&message));
                }
                Ok(LogOutput::StdErr { message }) => {
                    println!("stderr: {message:#?}");
                    stderr.write_all(&message).await?;
                    output.push_str(&String::from_utf8_lossy(&message));
                }
                Ok(LogOutput::StdIn { message }) => {
                    println!("stdin: {message:#?}");
                }
                Ok(LogOutput::Console { message }) => {
                    println!("console: {message:#?}");
                }
                Err(e) => {
                    dbg!(&e);
                }
            };
        }
        Ok::<(), ExecError>(())
    })
    .await??;

    let options = Some(InspectContainerOptions { size: false });
    let inspect_response = docker.inspect_container(&name, options).await?;

    let mut duration = None;
    if let Some(state) = inspect_response.state {
        if let Some(exit_code) = state.exit_code {
            if exit_code != 0 {
                return Err(ExecError::NonZeroExitCode(exit_code, output));
            }
        }

        if state.started_at.is_some() && state.finished_at.is_some() {
            let now = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east(0));
            let start = chrono::DateTime::parse_from_rfc3339(state.started_at.as_ref().unwrap())
                .unwrap_or(now);
            let end = chrono::DateTime::parse_from_rfc3339(state.finished_at.as_ref().unwrap())
                .unwrap_or(now);
            duration = (end - start).to_std().ok();
        }
    }

    Ok(duration.unwrap_or_default())
}

#[post("/exec_and_wait", data = "<req>")]
async fn exec_and_wait(
    req: Form<ExecAndWaitRequest>,
    config: &State<RunnerConfig>,
) -> Json<ExecAndWaitResponse> {
    let rep = exec_and_wait_inner(&req, config).await;
    let response = match rep {
        Ok(duration) => ExecAndWaitResponse {
            key: req.key.clone(),
            params: req.params.clone(),
            status: "OK".into(),
            error: String::new(),
            algo_info: AlgoInfo {
                error_message: String::new(),
                run_time: Some(duration.as_secs_f64()),
            },
        },
        Err(err) => match err {
            ExecError::Timeout(_) => ExecAndWaitResponse {
                key: req.key.clone(),
                params: req.params.clone(),
                status: "KO".into(),
                error: "IPOLTimeoutError".into(),
                algo_info: AlgoInfo {
                    error_message: err.to_string(),
                    run_time: None,
                },
            },
            _ => ExecAndWaitResponse {
                key: req.key.clone(),
                params: req.params.clone(),
                status: "KO".into(),
                error: err.to_string(),
                algo_info: AlgoInfo {
                    error_message: err.to_string(),
                    run_time: None,
                },
            },
        },
    };
    Json(response)
}

#[get("/")]
fn index() -> &'static str {
    "This is the IPOL DemoRunner module (docker)"
}

#[launch]
fn rocket() -> _ {
    // TODO: restrict access to the service somehow
    rocket::build()
        .mount(
            "/api/demorunner/",
            routes![
                index,
                ping,
                shutdown,
                get_workload,
                ensure_compilation,
                exec_and_wait
            ],
        )
        .mount(
            "/api/demorunner-docker/",
            routes![
                index,
                ping,
                shutdown,
                get_workload,
                ensure_compilation,
                exec_and_wait
            ],
        )
        .attach(AdHoc::config::<RunnerConfig>())
}

#[cfg(test)]
mod test {
    use super::rocket;
    use super::*;
    use rocket::http::{ContentType, Status};
    use rocket::local::blocking::Client;

    const GIT_URL: &str = "https://github.com/kidanger/ipol-demo-zero";

    // TODO: remove git repositories and docker images

    #[test]
    fn test_get_workfload() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");
        let response = client.get("/api/demorunner/get_workload").dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(
            response.into_json(),
            Some(Workload {
                status: "OK".into(),
                workload: 1.0,
            })
        );
    }

    #[test]
    fn test_ensure_compilation() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");
        let ddl_build = DDLBuild {
            url: GIT_URL.into(),
            rev: "69b4dbc2ff9c3102c3b86639ed1ab608a6b5ba79".into(),
            dockerfile: ".ipol/Dockerfile".into(),
        };
        let response = client
            .post("/api/demorunner/ensure_compilation")
            .header(rocket::http::ContentType::Form)
            .body(format!(
                "demo_id={}&ddl_build={}",
                "t001",
                serde_json::to_string(&ddl_build).unwrap()
            ))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(
            response.into_json(),
            Some(EnsureCompilationResponse {
                status: "OK".into(),
                message: "".into(),
                buildlog: None,
            })
        );
    }

    #[test]
    fn test_ensure_compilation_missing_dockerfile() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");
        let ddl_build = DDLBuild {
            url: GIT_URL.into(),
            rev: "69b4dbc2ff9c3102c3b86639ed1ab608a6b5ba79".into(),
            dockerfile: "missing".into(),
        };
        let response = client
            .post("/api/demorunner/ensure_compilation")
            .header(rocket::http::ContentType::Form)
            .body(format!(
                "demo_id={}&ddl_build={}",
                "t002",
                serde_json::to_string(&ddl_build).unwrap()
            ))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(
            response.into_json(),
            Some(EnsureCompilationResponse {
                status: "KO".into(),
                message: "Couldn't find dockerfile: missing".into(),
                buildlog: None,
            })
        );
    }

    #[test]
    fn test_ensure_compilation_invalid_git_commit() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");
        let ddl_build = DDLBuild {
            url: GIT_URL.into(),
            rev: "invalid".into(),
            dockerfile: ".ipol/Dockerfile".into(),
        };
        let response = client
            .post("/api/demorunner/ensure_compilation")
            .header(rocket::http::ContentType::Form)
            .body(format!(
                "demo_id={}&ddl_build={}",
                "t003",
                serde_json::to_string(&ddl_build).unwrap()
            ))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(
            response.into_json(),
            Some(EnsureCompilationResponse {
                status: "KO".into(),
                message: "revspec 'invalid' not found; class=Reference (4); code=NotFound (-3)"
                    .into(),
                buildlog: None,
            })
        );
    }

    #[test]
    fn test_ensure_compilation_invalid_dockerfile() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");
        let ddl_build = DDLBuild {
            url: GIT_URL.into(),
            rev: "69b4dbc2ff9c3102c3b86639ed1ab608a6b5ba79".into(),
            dockerfile: "Makefile".into(),
        };
        let response = client
            .post("/api/demorunner/ensure_compilation")
            .header(rocket::http::ContentType::Form)
            .body(format!(
                "demo_id={}&ddl_build={}",
                "t004",
                serde_json::to_string(&ddl_build).unwrap()
            ))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(
            response.into_json(),
            Some(EnsureCompilationResponse {
                status: "KO".into(),
                message: "Docker responded with status code 400: dockerfile parse error line 1: unknown instruction: CFLAGS=".into(),
                buildlog: None,
            })
        );
    }

    #[test]
    fn test_ensure_compilation_build_error() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");
        let ddl_build = DDLBuild {
            url: GIT_URL.into(),
            rev: "fe35687".into(),
            dockerfile: ".ipol/Dockerfile-error".into(),
        };
        let response = client
            .post("/api/demorunner/ensure_compilation")
            .header(rocket::http::ContentType::Form)
            .body(format!(
                "demo_id={}&ddl_build={}",
                "t005",
                serde_json::to_string(&ddl_build).unwrap()
            ))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);

        let r: EnsureCompilationResponse = response.into_json().unwrap();
        assert_eq!(r.status, "KO");
        assert_eq!(r.message, "Compilation error");
        assert!(!r.buildlog.unwrap().is_empty());
    }

    #[test]
    fn test_exec_and_wait() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");

        let key = "test_exec_and_wait".to_string();
        let params = RunParams::from([
            ("x".into(), ParamValue::PosInt(1)),
            ("y".into(), ParamValue::Float(2.5)),
            ("z".into(), ParamValue::String("t001".into())),
            ("a".into(), ParamValue::Bool(true)),
            ("b".into(), ParamValue::NegInt(-2)),
            ("param space".into(), ParamValue::String("hi world".into())),
        ]);
        let ddl_run = "test $z = $IPOL_DEMOID";
        let response = client
            .post("/api/demorunner/exec_and_wait")
            .header(ContentType::Form)
            .body(format!(
                "demo_id={}&key={}&params={}&ddl_run={}&timeout={}",
                "t001",
                key,
                serde_json::to_string(&params).unwrap(),
                serde_json::to_string(&ddl_run).unwrap(),
                10,
            ))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        let response: ExecAndWaitResponse = response.into_json().unwrap();
        assert_eq!(response.status, "OK");
        assert_eq!(response.key, key);
        assert_eq!(response.params, params);
        assert_eq!(response.error, "");
        assert_eq!(response.algo_info.error_message, "");
        assert!(response.algo_info.run_time.is_some());
        std::thread::sleep(Duration::from_secs(1));
    }

    #[test]
    fn test_exec_and_wait_non_zero_exit_code() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");

        let key = "test_exec_and_wait_non_zero_exit_code".to_string();
        let params = RunParams::new();
        let ddl_run = "echo a; exit 5; echo b;";
        let response = client
            .post("/api/demorunner/exec_and_wait")
            .header(ContentType::Form)
            .body(format!(
                "demo_id={}&key={}&params={}&ddl_run={}&timeout={}",
                "t001",
                key,
                serde_json::to_string(&params).unwrap(),
                serde_json::to_string(&ddl_run).unwrap(),
                10,
            ))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        let response: ExecAndWaitResponse = response.into_json().unwrap();
        assert_eq!(response.status, "KO");
        assert_eq!(response.key, key);
        assert_eq!(response.params, params);
        assert_eq!(response.error, "Non-zero exit code (5): a\n");
        assert_eq!(
            response.algo_info.error_message,
            "Non-zero exit code (5): a\n"
        );
        assert!(response.algo_info.run_time.is_none());
        std::thread::sleep(Duration::from_secs(1));
    }

    #[test]
    fn test_exec_and_wait_timeout() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");

        let key = "test_exec_and_wait_timeout".to_string();
        let params = RunParams::new();
        let ddl_run = "sleep 2";
        let response = client
            .post("/api/demorunner/exec_and_wait")
            .header(ContentType::Form)
            .body(format!(
                "demo_id={}&key={}&params={}&ddl_run={}&timeout={}",
                "t001",
                key,
                serde_json::to_string(&params).unwrap(),
                serde_json::to_string(&ddl_run).unwrap(),
                1,
            ))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        let response: ExecAndWaitResponse = response.into_json().unwrap();
        assert_eq!(response.status, "KO");
        assert_eq!(response.key, key);
        assert_eq!(response.params, params);
        assert_eq!(response.error, "IPOLTimeoutError");
        assert_eq!(
            response.algo_info.error_message,
            "IPOLTimeoutError: Execution timeout"
        );
        assert!(response.algo_info.run_time.is_none());
        std::thread::sleep(Duration::from_secs(1));
    }

    #[test]
    fn test_exec_and_wait_run_time() {
        let client = Client::tracked(rocket()).expect("valid rocket instance");

        let key = "test_exec_and_wait_run_time".to_string();
        let params = RunParams::new();
        let ddl_run = "sleep 2";
        let response = client
            .post("/api/demorunner/exec_and_wait")
            .header(ContentType::Form)
            .body(format!(
                "demo_id={}&key={}&params={}&ddl_run={}&timeout={}",
                "t001",
                key,
                serde_json::to_string(&params).unwrap(),
                serde_json::to_string(&ddl_run).unwrap(),
                10,
            ))
            .dispatch();
        assert_eq!(response.status(), Status::Ok);
        let response: ExecAndWaitResponse = response.into_json().unwrap();
        assert_eq!(response.status, "OK");
        assert_eq!(response.key, key);
        assert_eq!(response.params, params);
        assert_eq!(response.error, "");
        assert_eq!(response.algo_info.error_message, "");
        assert!(response.algo_info.run_time > Some(1.5));
        std::thread::sleep(Duration::from_secs(1));
    }

    #[test]
    fn test_url_of_git_repository() {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = tmpdir.path();
        let url = url_of_git_repository(path);
        assert_eq!(url, None);

        let commit = "69b4dbc2ff9c3102c3b86639ed1ab608a6b5ba79";
        let url1 = String::from(GIT_URL);
        let r = prepare_git(path, &url1, commit);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), commit);

        let url = url_of_git_repository(path);
        assert_eq!(url, Some(url1));

        let url2 = format!("{}.git", GIT_URL);
        let r = prepare_git(path, &url2, commit);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), commit);

        let url = url_of_git_repository(path);
        assert_eq!(url, Some(url2));
    }

    #[test]
    fn test_prepare_git_private() {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = tmpdir.path();

        let urls = std::env::var("PRIVATE_URLS").unwrap_or_default();
        for url in urls.split(',').filter(|x| !x.is_empty()) {
            dbg!(url);
            let r = prepare_git(path, url, "master");
            assert!(r.is_ok());
        }
    }
}
