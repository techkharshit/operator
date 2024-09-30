use anyhow::{Result};
//use reqwest::Client as ReqwestClient;
//use rustls::{ClientConfig, RootCertStore};
//use std::sync::Arc;
//use webpki_roots::TLS_SERVER_ROOTS;
use thiserror::Error;
//use reqwest::Client;
use std::process::{Command, Stdio};
use tokio::{time::{sleep, Duration}, io::{AsyncBufReadExt, BufReader}};
use tokio::process::Command as TokioCommand;
use kube_runtime::controller::{Context as ControllerContext, Controller, ReconcilerAction};
use serde::{Deserialize, Serialize};
use kube::{
    api::{Api, ListParams},
    Client as KubeClient, CustomResource, ResourceExt,
    Error as KubeError,
};
use tokio::task;
//use std::time::Duration as StdDuration;
use tracing::{info, warn};
use futures_util::StreamExt;
use schemars::JsonSchema;

#[derive(Debug, Error)]
pub enum MyError {
    #[error("Command error: {0}")]
    CommandError(#[from] std::io::Error),
    #[error("Kube error: {0}")]
    KubeError(#[from] kube::Error),
    #[error("Reqwest error: {0}")]
    ReqwestError(#[from] reqwest::Error),
    #[error("Serde error: {0}")]
    SerdeError(#[from] serde_json::Error),
    #[error("Other error: {0}")]
    Other(String),
}

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(group = "example.com", version = "v1", kind = "MyServer", namespaced)]
pub struct MyServerSpec {
    replicas: i32,
    version: String,
}

#[tokio::main]
async fn main() -> Result<(), KubeError> {
    tracing_subscriber::fmt::init();

    let client = KubeClient::try_default().await?;
    let api: Api<MyServer> = Api::all(client.clone());

    Controller::new(api, ListParams::default())
    .run(
        reconcile,
        |error, ctx| {
            error_policy(&error, ctx)
        },
        ControllerContext::new(()),
    )
    .for_each(|reconciliation_result| async move {
            match reconciliation_result {
                Ok(o) => info!("Reconciled {:?}", o),
                Err(e) => warn!("Reconciliation error: {:?}", e),
            }
        })
        .await;

    Ok(())
}

async fn reconcile(myserver: MyServer, _ctx: ControllerContext<()>) -> Result<ReconcilerAction, MyError> {
    println!("Reconciling MyServer: {:?}", myserver.name());
    // Removed start_minikube() call
    apply_kubernetes_manifests()?;
    start_kubectl_proxy().await?;
    port_forward_rust_server().await?;
    println!("over...");
    Ok(ReconcilerAction {
        requeue_after: Some(Duration::from_secs(60)),
    })
}

// Adjust the error_policy function signature
fn error_policy(_error: &MyError, _ctx: kube_runtime::controller::Context<()>) -> ReconcilerAction {
    ReconcilerAction {
        requeue_after: Some(Duration::from_secs(10)),
    }
}

fn apply_kubernetes_manifests() -> Result<(), MyError> {
    let manifests = [
        "k8s/mysql-pvc.yaml",
        "k8s/minio-pvc.yaml",
        "k8s/mysql-deployment.yaml",
        "k8s/minio-deployment.yaml",
        "k8s/rust-server-deployment.yaml",
        "k8s/mysql-init-script-configmap.yaml",
    ];

    for manifest in &manifests {
        println!("Applying manifest: {}", manifest);
        let output = Command::new("/usr/local/bin/kubectl")  // Ensure full path to kubectl
            .args(&["apply", "-f", manifest])
            .output()
            .map_err(MyError::from)?;

        println!("kubectl stdout: {}", String::from_utf8_lossy(&output.stdout));
        if !output.status.success() {
            let err_msg = String::from_utf8_lossy(&output.stderr);
            return Err(MyError::Other(format!("kubectl apply failed for {}: {}", manifest, err_msg)));
        }

        println!("Applied manifest: {}", manifest);
    }
    Ok(())
}

async fn start_kubectl_proxy() -> Result<(), MyError> {
    let mut cmd = TokioCommand::new("/usr/local/bin/kubectl")  // Ensure full path to kubectl
        .args(&["proxy", "--port=8001"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = cmd.stdout.take().ok_or_else(|| MyError::Other("Failed to capture stdout".into()))?;
    let stderr = cmd.stderr.take().ok_or_else(|| MyError::Other("Failed to capture stderr".into()))?;
    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    tokio::spawn(async move {
        while let Some(line) = stdout_reader.next_line().await.ok() {
            if let Some(content) = line {
                println!("stdout: {}", content);
                if content.contains("Starting to serve on") {
                    println!("kubectl proxy is active on port 8001.");
                    sleep(Duration::from_secs(5)).await;
                    break;
                }
            }
        }
    });

    tokio::spawn(async move {
        while let Some(line) = stderr_reader.next_line().await.ok() {
            if let Some(content) = line {
                eprintln!("stderr: {}", content);
                if content.contains("error") {
                    eprintln!("kubectl proxy failed: {}", content);
                    std::process::exit(1);
                }
            }
        }
    });

    Ok(())
}


async fn port_forward_rust_server() -> Result<(), MyError> {
    // Ensure all required pods are ready
    wait_for_all_pods_ready("default").await?;

    // Start the kubectl port-forward command as a background task
    let port_forward_task = task::spawn(async {
        let mut cmd = TokioCommand::new("kubectl")
            .args(&[
                "port-forward",
                "svc/rust-server",   // Target the service
                "8000:8000",         // Map local port 8000 to service port 8000
                "-n", "default",      // Specify the namespace
                "--address", "0.0.0.0"  // Bind to all network interfaces to allow external access
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| MyError::Other(format!("Failed to spawn kubectl port-forward: {:?}", e)))?;

        // Capture stdout and stderr streams
        let stdout = cmd.stdout.take().ok_or_else(|| MyError::Other("Failed to capture stdout".into()))?;
        let stderr = cmd.stderr.take().ok_or_else(|| MyError::Other("Failed to capture stderr".into()))?;
        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        // Monitor the output to ensure the port-forward is active
        let mut port_forward_active = false;

        while let Ok(Some(line_result)) = tokio::select! {
            line_result = stdout_reader.next_line() => line_result,
            line_result = stderr_reader.next_line() => line_result,
        } {
            println!("stdout: {}", line_result);
            if line_result.contains("Forwarding from 0.0.0.0:8000 -> 8000") {
                println!("Port forwarding is active on port 8000.");
                port_forward_active = true;
                break;
            }

            // Handle potential errors during port-forwarding
            if line_result.contains("error") {
                return Err(MyError::Other(format!("Port-forwarding error: {}", line_result)));
            }

            // Sleep to avoid looping too frequently
            sleep(Duration::from_secs(1)).await;
        }

        // Ensure port forwarding is active
        if !port_forward_active {
            return Err(MyError::Other("Port-forwarding did not become active".into()));
        }

        // Wait for the kubectl command to complete
        let status = cmd.wait().await?;
        if !status.success() {
            return Err(MyError::Other(format!("Port-forwarding process exited with status: {}", status)));
        }

        println!("Port-forwarding process completed.");
        Ok(())
    });

    // Wait for the port-forward task to finish (this will keep the function alive)
    match port_forward_task.await {
        Ok(result) => result,
        Err(e) => Err(MyError::Other(format!("Port-forward task failed: {:?}", e))),
    }
}







async fn wait_for_all_pods_ready(namespace: &str) -> Result<(), MyError> {
    let client = reqwest::Client::new();
    let kube_api_url = format!("http://localhost:8001/api/v1/namespaces/{}/pods", namespace);

    loop {
        match client.get(&kube_api_url).send().await {
            Ok(response) if response.status().is_success() => {
                let body = response.text().await?;
                let json: serde_json::Value = serde_json::from_str(&body)?;
                if let Some(pods) = json["items"].as_array() {
                    if pods.is_empty() {
                        println!("No pods found in the response. Retrying...");
                    } else {
                        let empty_vec = vec![];
                        for pod in pods {
                            let name = pod["metadata"]["name"].as_str().unwrap_or("");
                            let phase = pod["status"]["phase"].as_str().unwrap_or("");
                            let conditions = pod["status"]["conditions"].as_array().unwrap_or(&empty_vec);
                            let ready_condition = conditions.iter().find(|c| c["type"] == "Ready");
                            let ready_status = ready_condition.map_or("False", |c| c["status"].as_str().unwrap_or("False"));

                            println!("Pod: {}, Phase: {}, Ready: {}", name, phase, ready_status);
                        }

                        let all_pods_ready = pods.iter().all(|pod| {
                            let is_running = pod["status"]["phase"] == "Running";
                            let is_ready = pod["status"]["conditions"].as_array().map_or(false, |conditions| {
                                conditions.iter().any(|condition| {
                                    condition["type"] == "Ready" && condition["status"] == "True"
                                })
                            });

                            is_running && is_ready
                        });
                        if all_pods_ready {
                            println!("All required pods are Ready and Running.");
                            break;
                        } else {
                            println!("Waiting for all required pods to be Ready and Running...");
                        }
                    }
                }
            },
            Ok(response) => {
                println!("Failed to get pod status from Kubernetes API with status code: {}", response.status());
            }
            Err(e) => {
                println!("Error querying Kubernetes API: {}. Retrying in 5 seconds...", e);
            }
        }

        sleep(Duration::from_secs(5)).await;
    }

    Ok(())
}