//! Job: the receipt for one execution — the graded runs of one batch.

use crate::run::Run;
use indexmap::IndexMap;

/// The receipt for one execution: the graded runs under one job id.
#[derive(Default)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub runs: Vec<Run>,
    pub group: u32,
    /// Platform taskset id this job runs, when it came from a synced taskset.
    pub taskset_id: Option<String>,
}

impl Job {
    /// Open a job spanning multiple scheduler calls: pass it as
    /// `RunOptions::job` to accumulate every run of a longer arc under one id.
    pub fn start(name: impl Into<String>, group: u32) -> Job {
        Job {
            id: uuid::Uuid::new_v4().simple().to_string(),
            name: name.into(),
            runs: Vec::new(),
            group,
            taskset_id: None,
        }
    }

    /// Mean reward across runs (0.0 for an empty job).
    pub fn reward(&self) -> f64 {
        if self.runs.is_empty() {
            return 0.0;
        }
        self.runs.iter().map(Run::reward).sum::<f64>() / self.runs.len() as f64
    }

    /// Runs grouped by task slug — the safe alternative to positional zip
    /// (list-valued because `group > 1` produces several runs per task).
    pub fn results(&self) -> IndexMap<String, Vec<&Run>> {
        let mut out: IndexMap<String, Vec<&Run>> = IndexMap::new();
        for run in &self.runs {
            out.entry(run.slug.clone().unwrap_or_default())
                .or_default()
                .push(run);
        }
        out
    }
}
