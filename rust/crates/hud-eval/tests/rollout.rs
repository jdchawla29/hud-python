//! End-to-end rollout tests against an in-process Rust env server.

use hud_env::{template, BoundServer, Environment, Evaluation, Prompt};
use hud_eval::{
    agent_fn, rollout, Agent, Provider, ProviderError, RolloutOptions, RunOptions, Runtime,
    RuntimeGuard, TaskRow, Taskset,
};
use hud_types::{StepSource, TaskPhase, TraceStatus};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[derive(Deserialize)]
struct EchoArgs {
    text: String,
}

async fn serve_env() -> (BoundServer, Runtime) {
    let mut env = Environment::new("echo-env");
    env.add_template(template(
        "echo",
        "Repeat the text exactly.",
        |args: EchoArgs| async move {
            let expected = args.text.clone();
            Ok((
                Prompt::Text(format!("Repeat exactly: {}", args.text)),
                expected,
            ))
        },
        |expected: String, answer| async move {
            Ok(Evaluation::Score(if answer.text().trim() == expected {
                1.0
            } else {
                0.0
            }))
        },
    ));
    env.add_template(template(
        "always-fail-grade",
        "",
        |_: serde_json::Map<String, serde_json::Value>| async move {
            Ok((Prompt::Text("go".into()), ()))
        },
        |_: (), _| async move { Err("grader exploded".into()) },
    ));
    let mut env = env;
    env.start().await.unwrap();
    let server = hud_env::bind(Arc::new(env), "127.0.0.1", 0).await.unwrap();
    let runtime = Runtime::new(format!("tcp://127.0.0.1:{}", server.port()));
    (server, runtime)
}

/// A per-acquisition provider (the in-process analog of `LocalRuntime`):
/// each rollout gets a fresh env server, so concurrent rollouts never share
/// the one-task-per-server session.
struct InProcessProvider;

#[async_trait::async_trait]
impl Provider for InProcessProvider {
    async fn acquire(&self, _task: &TaskRow) -> Result<RuntimeGuard, ProviderError> {
        let (server, runtime) = serve_env().await;
        Ok(RuntimeGuard::with_teardown(
            runtime,
            Box::pin(async move { server.shutdown().await }),
        ))
    }
}

fn echo_task(text: &str) -> TaskRow {
    let mut args = serde_json::Map::new();
    args.insert("text".to_string(), json!(text));
    TaskRow::new("echo-env", "echo").with_args(args)
}

/// An agent that answers with the prompt's tail after "Repeat exactly: ".
fn echo_agent() -> impl Agent {
    agent_fn(|run| {
        Box::pin(async move {
            let prompt = run.prompt_text();
            let answer = prompt
                .strip_prefix("Repeat exactly: ")
                .unwrap_or(&prompt)
                .to_string();
            run.trace.content = Some(answer);
            Ok(())
        })
    })
}

#[tokio::test]
async fn rollout_grades_and_records_lifecycle_steps() {
    let (_server, runtime) = serve_env().await;
    let task = echo_task("hello world");

    let run = rollout(&task, &echo_agent(), &runtime, RolloutOptions::default()).await;

    assert_eq!(run.reward(), 1.0);
    assert!(run.grade.done);
    assert_eq!(run.trace.status, Some(TraceStatus::Completed));
    assert_eq!(run.runtime(), Some(runtime.url.as_str()));
    assert!(run.trace_id().is_some());
    assert!(run.job_id.is_some());
    assert_eq!(run.slug.as_deref(), Some(task.effective_slug().as_str()));

    // Steps: setup task call, user prompt, evaluate task call.
    let phases: Vec<TaskPhase> = run.trace.collect(|s| s.task_call.as_ref().map(|c| c.phase));
    assert_eq!(phases, vec![TaskPhase::Setup, TaskPhase::Evaluate]);
    let sources: Vec<StepSource> = run.trace.steps.iter().map(|s| s.source).collect();
    assert_eq!(
        sources,
        vec![StepSource::Task, StepSource::User, StepSource::Task]
    );
    assert_eq!(run.evaluation()["score"], json!(1.0));
}

#[tokio::test]
async fn wrong_answer_scores_zero() {
    let (_server, runtime) = serve_env().await;
    let agent = agent_fn(|run| {
        Box::pin(async move {
            run.trace.content = Some("wrong".to_string());
            Ok(())
        })
    });
    let run = rollout(
        &echo_task("expected"),
        &agent,
        &runtime,
        RolloutOptions::default(),
    )
    .await;
    assert_eq!(run.reward(), 0.0);
    assert_eq!(run.trace.status, Some(TraceStatus::Completed));
}

#[tokio::test]
async fn provision_failure_synthesizes_failed_run() {
    // Nothing serves on this runtime; connect exhausts its ready timeout.
    let mut runtime = Runtime::new("tcp://127.0.0.1:1");
    runtime
        .params
        .insert("ready_timeout".to_string(), json!(0.3));

    let run = rollout(
        &echo_task("x"),
        &echo_agent(),
        &runtime,
        RolloutOptions::default(),
    )
    .await;
    assert!(!run.has_client());
    assert_eq!(run.trace.status, Some(TraceStatus::Error));
    let error = run.trace.error().unwrap();
    assert!(error.starts_with("[starting task]"), "unexpected: {error}");
}

#[tokio::test]
async fn agent_error_still_grades_best_effort() {
    let (_server, runtime) = serve_env().await;
    let agent = agent_fn(|run| {
        Box::pin(async move {
            run.trace.content = Some("hello".to_string());
            Err("agent blew up".into())
        })
    });
    let run = rollout(
        &echo_task("hello"),
        &agent,
        &runtime,
        RolloutOptions::default(),
    )
    .await;

    // Status stays error, but the salvageable reward was captured.
    assert_eq!(run.trace.status, Some(TraceStatus::Error));
    assert_eq!(run.reward(), 1.0);
    let error = run.trace.error().unwrap();
    assert!(
        error.contains("[agent loop] agent blew up"),
        "unexpected: {error}"
    );
}

#[tokio::test]
async fn grader_fault_marks_run_errored() {
    let (_server, runtime) = serve_env().await;
    let task = TaskRow::new("echo-env", "always-fail-grade");
    let run = rollout(&task, &echo_agent(), &runtime, RolloutOptions::default()).await;
    assert_eq!(run.trace.status, Some(TraceStatus::Error));
    let error = run.trace.error().unwrap();
    assert!(error.starts_with("[grading]"), "unexpected: {error}");
    assert!(error.contains("grader exploded"), "unexpected: {error}");
}

#[tokio::test]
async fn agent_timeout_truncates_and_grades_partial() {
    let (_server, runtime) = serve_env().await;
    let agent = agent_fn(|run| {
        Box::pin(async move {
            run.trace.content = Some("hello".to_string());
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(())
        })
    });
    let run = rollout(
        &echo_task("hello"),
        &agent,
        &runtime,
        RolloutOptions {
            rollout_timeout: Some(Duration::from_millis(800)),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(run.trace.extra["stop_reason"], json!("timeout"));
    // The partial answer was graded on the normal path.
    assert_eq!(run.reward(), 1.0);
    assert_eq!(run.trace.status, Some(TraceStatus::Completed));
}

/// The whole engine against the reference Python SDK (this repo's root
/// project): `LocalRuntime` spawns `uv run python -m hud.environment.server`,
/// reads the port announcement, and `rollout` drives the Python-served task
/// to a graded run.
#[tokio::test]
async fn local_runtime_rollout_against_python_env() {
    let python_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/interop/fixture_env.py");

    let provider = hud_eval::LocalRuntime::command([
        "uv".to_string(),
        "run".to_string(),
        "--project".to_string(),
        python_dir.to_string_lossy().into_owned(),
        "python".to_string(),
        "-m".to_string(),
        "hud.environment.server".to_string(),
        fixture.to_string_lossy().into_owned(),
    ]);

    let run = rollout(
        &echo_task("across the wire"),
        &echo_agent(),
        &provider,
        RolloutOptions::default(),
    )
    .await;

    assert_eq!(
        run.trace.status,
        Some(TraceStatus::Completed),
        "trace error: {:?}",
        run.trace.error()
    );
    assert_eq!(run.reward(), 1.0);
    assert_eq!(run.prompt_text(), "Repeat exactly: across the wire");
}

#[tokio::test]
async fn taskset_run_expands_groups_and_caps_concurrency() {
    let taskset = Taskset::new(
        "echoes",
        vec![echo_task("alpha"), echo_task("beta"), echo_task("gamma")],
    )
    .unwrap();

    let job = taskset
        .run(
            &echo_agent(),
            &InProcessProvider,
            RunOptions {
                group: 2,
                max_concurrent: Some(2),
                ..Default::default()
            },
        )
        .await;

    assert_eq!(job.runs.len(), 6);
    assert_eq!(job.reward(), 1.0);
    assert_eq!(job.group, 2);
    assert_eq!(job.name, "echoes (3 tasks) (2 times)");

    // Task-major expansion: each task's group shares one group_id.
    let results = job.results();
    assert_eq!(results.len(), 3);
    for (_, runs) in results {
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].group_id, runs[1].group_id);
        assert!(runs[0].group_id.is_some());
    }
    // Different tasks get different group ids.
    let group_ids: std::collections::HashSet<_> =
        job.runs.iter().filter_map(|r| r.group_id.clone()).collect();
    assert_eq!(group_ids.len(), 3);
    // All runs share the job.
    assert!(job
        .runs
        .iter()
        .all(|r| r.job_id.as_deref() == Some(job.id.as_str())));
}
