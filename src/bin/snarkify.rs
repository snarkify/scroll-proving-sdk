use async_trait::async_trait;
use clap::Parser;
use core::time::Duration;
use reqwest::{header::CONTENT_TYPE, Url};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};

use scroll_proving_sdk::{
    config::{CloudProverConfig, Config},
    prover::{
        proving_service::{
            GetVkRequest, GetVkResponse, ProveRequest, ProveResponse, QueryTaskRequest,
            QueryTaskResponse, TaskStatus,
        },
        types::CircuitType,
        ProverBuilder, ProvingService,
    },
    utils::init_tracing,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[clap(disable_version_flag = true)]
struct Args {
    /// Path of config file
    #[arg(long = "config", default_value = "config.json")]
    config_file: String,
    /// Service ID
    #[arg(long = "service-id")]
    service_id: String,
}

#[derive(Deserialize, Debug)]
pub struct SnarkifyGetTaskResponse {
    pub task_id: String,
    pub created: DateTime<Utc>,
    pub started: Option<DateTime<Utc>>,
    pub finished: Option<DateTime<Utc>>,
    pub state: SnarkifyTaskState,
    pub input: String,
    pub proof: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize, Debug)]
pub enum SnarkifyTaskState {
    Pending,
    Success,
    Failure,
}

impl From<SnarkifyTaskState> for TaskStatus {
    fn from(status: SnarkifyTaskState) -> Self {
        match status {
            SnarkifyTaskState::Pending => TaskStatus::Proving,
            SnarkifyTaskState::Success => TaskStatus::Success,
            SnarkifyTaskState::Failure => TaskStatus::Failed,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SnarkifyCreateTaskInput {
    pub circuit_type: CircuitType,
    pub circuit_version: String,
    pub hard_fork_name: String,
    pub task_data: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SnarkifyCreateTaskRequest {
    pub input: SnarkifyCreateTaskInput,
}

impl SnarkifyCreateTaskRequest {
    pub fn from_prove_request(request: &ProveRequest) -> Self {
        Self {
            input: SnarkifyCreateTaskInput {
                circuit_type: request.circuit_type,
                circuit_version: request.circuit_version.clone(),
                hard_fork_name: request.hard_fork_name.clone(),
                task_data: request.input.clone(),
            },
        }
    }
}

struct SnarkifyProver {
    base_url: Url,
    api_key: String,
    service_id: String,
    send_timeout: Duration,
    client: ClientWithMiddleware,
}

#[async_trait]
impl ProvingService for SnarkifyProver {
    fn is_local(&self) -> bool {
        false
    }
    async fn get_vk(&self, req: GetVkRequest) -> GetVkResponse {
        // TODO: Send a request to get the VK
        GetVkResponse {
            vk: "AAAAGQAAAATyWEABRbJ6hQQ5/zLX1gTasr7349minA9rSgMS6gDeHwZKqikRiO3md+pXjjxMHnKQtmXYgMXhJSvlmZ+Ws+cheuly2X1RuNQzcZuRImaKPR9LJsVZYsXfJbuqdKX8p0Gj8G83wMJOmTzNVUyUol0w0lTU+CEiTpHOnxBsTF3EWaW3s1u4ycOgWt1c9M6s7WmaBZLYgAWYCunO5CLCLApNGbCASeck/LuSoedEri5u6HccCKU2khG6zl6W07jvYSbDVLJktbjRiHv+/HQix+K14j8boo8Z/unhpwXCsPxkQA==".to_string(),
            error: None,
        }
    }
    async fn prove(&self, req: ProveRequest) -> ProveResponse {
        let body = SnarkifyCreateTaskRequest::from_prove_request(&req);
        let method = format!("/services/{}", &self.service_id);

        match self
            .post_with_token::<SnarkifyCreateTaskRequest, SnarkifyGetTaskResponse>(&method, &body)
            .await
        {
            Ok(resp) => ProveResponse {
                task_id: resp.task_id,
                circuit_type: req.circuit_type,
                circuit_version: req.circuit_version,
                hard_fork_name: req.hard_fork_name,
                status: resp.state.into(),
                created_at: resp.created.timestamp() as f64,
                started_at: resp.started.map(|t| t.timestamp() as f64),
                finished_at: None,
                compute_time_sec: None,
                input: Some(req.input.clone()),
                proof: None,
                vk: None,
                error: None,
            },
            Err(e) => {
                return self
                    .build_prove_error_response(&req, &format!("Failed to request proof: {}", e))
            }
        }
    }

    async fn query_task(&self, req: QueryTaskRequest) -> QueryTaskResponse {
        let method = format!("/tasks/{}", &req.task_id);
        match self
            .get_with_token::<SnarkifyGetTaskResponse>(&method)
            .await
        {
            Ok(resp) => {
                let task_input: SnarkifyCreateTaskInput = match serde_json::from_str(&resp.input) {
                    Ok(input) => input,
                    Err(e) => {
                        return self.build_query_task_error_response(
                            &req,
                            &format!("Failed to parse task input: {}", e),
                        )
                    }
                };
                let started_at = resp.started.map(|t| t.timestamp() as f64);
                let finished_at = resp.finished.map(|t| t.timestamp() as f64);
                let compute_time_sec = match (started_at, finished_at) {
                    (Some(started), Some(finished)) => Some(finished - started),
                    _ => None,
                };
                QueryTaskResponse {
                    task_id: resp.task_id,
                    circuit_type: task_input.circuit_type,
                    circuit_version: task_input.circuit_version,
                    hard_fork_name: task_input.hard_fork_name,
                    status: resp.state.into(),
                    created_at: resp.created.timestamp() as f64,
                    started_at,
                    finished_at,
                    compute_time_sec,
                    input: Some(task_input.task_data),
                    proof: resp.proof,
                    vk: None,
                    error: resp.error,
                }
            }
            Err(e) => {
                self.build_query_task_error_response(&req, &format!("Failed to query proof: {}", e))
            }
        }
    }
}

impl SnarkifyProver {
    pub fn new(cfg: CloudProverConfig, service_id: String) -> Self {
        let retry_wait_duration = Duration::from_secs(cfg.retry_wait_time_sec);
        let retry_policy = ExponentialBackoff::builder()
            .retry_bounds(retry_wait_duration / 2, retry_wait_duration)
            .build_with_max_retries(cfg.retry_count);
        let client = ClientBuilder::new(reqwest::Client::new())
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();
        let base_url = Url::parse(&cfg.base_url).expect("Cannot parse cloud prover base_url");

        Self {
            base_url,
            api_key: cfg.api_key,
            service_id,
            send_timeout: Duration::from_secs(cfg.connection_timeout_sec),
            client,
        }
    }

    pub fn build_prove_error_response(&self, req: &ProveRequest, error_msg: &str) -> ProveResponse {
        ProveResponse {
            task_id: String::new(),
            circuit_type: req.circuit_type,
            circuit_version: req.circuit_version.clone(),
            hard_fork_name: req.hard_fork_name.clone(),
            status: TaskStatus::Failed,
            created_at: 0.0,
            started_at: None,
            finished_at: None,
            compute_time_sec: None,
            input: Some(req.input.clone()),
            proof: None,
            vk: None,
            error: Some(error_msg.to_string()),
        }
    }

    pub fn build_query_task_error_response(
        &self,
        req: &QueryTaskRequest,
        error_msg: &str,
    ) -> QueryTaskResponse {
        QueryTaskResponse {
            task_id: req.task_id.clone(),
            circuit_type: CircuitType::Undefined,
            circuit_version: "".to_string(),
            hard_fork_name: "".to_string(),
            status: TaskStatus::Queued,
            created_at: 0.0,
            started_at: None,
            finished_at: None,
            compute_time_sec: None,
            input: None,
            proof: None,
            vk: None,
            error: Some(error_msg.to_string()),
        }
    }

    fn build_url(&self, method: &str) -> anyhow::Result<Url> {
        self.base_url.join(method).map_err(|e| anyhow::anyhow!(e))
    }

    async fn get_with_token<Resp>(&self, method: &str) -> anyhow::Result<Resp>
    where
        Resp: serde::de::DeserializeOwned,
    {
        let url = self.build_url(method)?;
        log::info!("[Snarkify Client], {method}, sent request");
        let response = self
            .client
            .get(url)
            .header(CONTENT_TYPE, "application/json")
            .header("X-Api-Key", &self.api_key)
            .timeout(self.send_timeout)
            .send()
            .await?;

        let status = response.status();
        if !(status >= http::status::StatusCode::OK && status <= http::status::StatusCode::ACCEPTED)
        {
            anyhow::bail!("[Snarkify Client], {method}, status not ok: {}", status)
        }

        let response_body = response.text().await?;

        log::info!("[Snarkify Client], {method}, received response");
        log::debug!("[Snarkify Client], {method}, response: {response_body}");
        serde_json::from_str(&response_body).map_err(|e| anyhow::anyhow!(e))
    }

    async fn post_with_token<Req, Resp>(&self, method: &str, req: &Req) -> anyhow::Result<Resp>
    where
        Req: ?Sized + Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        let url = self.build_url(method)?;
        let request_body = serde_json::to_string(req)?;
        log::info!("[Snarkify Client], {method}, sent request");
        log::debug!("[Snarkify Client], {method}, request: {request_body}");
        let response = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header("X-Api-Key", &self.api_key)
            .body(request_body)
            .timeout(self.send_timeout)
            .send()
            .await?;

        let status = response.status();
        if !(status >= http::status::StatusCode::OK && status <= http::status::StatusCode::ACCEPTED)
        {
            anyhow::bail!("[Snarkify Client], {method}, status not ok: {}", status)
        }

        let response_body = response.text().await?;

        log::info!("[Snarkify Client], {method}, received response");
        log::debug!("[Snarkify Client], {method}, response: {response_body}");
        serde_json::from_str(&response_body).map_err(|e| anyhow::anyhow!(e))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let args = Args::parse();
    let cfg: Config = Config::from_file(args.config_file)?;
    let cloud_prover = SnarkifyProver::new(
        cfg.prover
            .cloud
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Missing cloud prover configuration"))?,
        args.service_id,
    );
    let prover = ProverBuilder::new(cfg)
        .with_proving_service(Box::new(cloud_prover))
        .build()
        .await?;

    prover.run().await;

    Ok(())
}
