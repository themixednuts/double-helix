use helix_pkg::{OpEvent, Ops};

use crate::runtime::{RuntimeIngress, RuntimeTaskEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgOperation {
    Install(Vec<String>),
    Update(Vec<String>),
    Remove(Vec<String>),
    Sync,
    Doctor,
}

pub fn spawn(operation: PkgOperation, work: helix_runtime::Work, ingress: RuntimeIngress) {
    work.spawn(async move {
        let ingress_for_events = ingress.clone();
        let result =
            tokio::task::spawn_blocking(move || run_blocking(operation, ingress_for_events))
                .await
                .map_err(|err| anyhow::anyhow!("package task failed: {err}"))
                .and_then(|result| result);

        if let Err(err) = result {
            ingress.task(RuntimeTaskEvent::PkgEvent(OpEvent::Failed {
                name: "pkg".to_owned(),
                message: err.to_string(),
            }));
        }
    })
    .detach();
}

fn run_blocking(operation: PkgOperation, ingress: RuntimeIngress) -> anyhow::Result<()> {
    let ops = Ops::open_default()?;
    let mut progress = |event: OpEvent| {
        ingress.task(RuntimeTaskEvent::PkgEvent(event));
    };

    match operation {
        PkgOperation::Install(names) => {
            ops.install(&names, &mut progress)?;
        }
        PkgOperation::Update(names) => {
            ops.update(&names, &mut progress)?;
        }
        PkgOperation::Remove(names) => {
            for name in &names {
                progress(OpEvent::Started { name: name.clone() });
            }
            ops.remove(&names)?;
            for name in names {
                progress(OpEvent::Done { name });
            }
        }
        PkgOperation::Sync => {
            ops.sync(&mut progress)?;
        }
        PkgOperation::Doctor => {
            progress(OpEvent::Started {
                name: "doctor".to_owned(),
            });
            let report = ops.doctor()?;
            progress(OpEvent::Progress {
                name: "doctor".to_owned(),
                message: format!("{} ok, {} problems", report.ok.len(), report.bad.len()),
            });
            progress(OpEvent::Done {
                name: "doctor".to_owned(),
            });
        }
    }

    Ok(())
}
