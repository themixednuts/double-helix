use helix_pkg::{ops::Progress, OpEvent, Ops};

use crate::runtime::{RuntimeIngress, RuntimeTaskEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgOperation {
    Install(Vec<String>),
    Update(Vec<String>),
    Remove(Vec<String>),
    Sync,
    Doctor,
    DoctorPackage(String),
    Rollback(String),
    UpdateRegistries(Vec<String>),
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
            run_name_batch(&names, &mut progress, |name, progress| {
                ops.install(&[name.to_owned()], progress).map(|_| ())
            })?;
        }
        PkgOperation::Update(names) => {
            if names.is_empty() {
                ops.update(&names, &mut progress)?;
            } else {
                run_name_batch(&names, &mut progress, |name, progress| {
                    ops.update(&[name.to_owned()], progress).map(|_| ())
                })?;
            }
        }
        PkgOperation::Remove(names) => {
            run_name_batch(&names, &mut progress, |name, progress| {
                progress(OpEvent::Started {
                    name: name.to_owned(),
                });
                ops.remove(&[name.to_owned()])?;
                progress(OpEvent::Done {
                    name: name.to_owned(),
                });
                Ok(())
            })?;
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
                percent: None,
            });
            progress(OpEvent::Done {
                name: "doctor".to_owned(),
            });
        }
        PkgOperation::DoctorPackage(name) => {
            progress(OpEvent::Started { name: name.clone() });
            let report = ops.doctor()?;
            if let Some((_, message)) = report.bad.iter().find(|(bad, _)| bad == &name) {
                progress(OpEvent::Failed {
                    name,
                    message: message.clone(),
                });
            } else {
                progress(OpEvent::Progress {
                    name: name.clone(),
                    message: "doctor ok".to_owned(),
                    percent: Some(100),
                });
                progress(OpEvent::Done { name });
            }
        }
        PkgOperation::Rollback(name) => {
            progress(OpEvent::Started { name: name.clone() });
            ops.rollback(&name)?;
            progress(OpEvent::Done { name });
        }
        PkgOperation::UpdateRegistries(names) => {
            let label = if names.is_empty() {
                "registries".to_owned()
            } else {
                names.join(",")
            };
            progress(OpEvent::Started {
                name: label.clone(),
            });
            let updates = ops.update_registries(&names)?;
            progress(OpEvent::Progress {
                name: label.clone(),
                message: format!("{} source(s) updated", updates.len()),
                percent: Some(100),
            });
            progress(OpEvent::Done { name: label });
        }
    }

    Ok(())
}

fn run_name_batch(
    names: &[String],
    progress: &mut Progress<'_>,
    mut run: impl FnMut(&str, &mut Progress<'_>) -> helix_pkg::Result<()>,
) -> anyhow::Result<()> {
    for name in names {
        if let Err(err) = run(name, progress) {
            progress(OpEvent::Failed {
                name: name.clone(),
                message: err.to_string(),
            });
        }
    }
    Ok(())
}
