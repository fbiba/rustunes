//! Terminal rendering belongs to the command-line frontend, never recovery proofs.
use crate::engine::recovery::resume::{Kind, Report};
use anyhow::Result;
use std::io::Write;

pub(super) fn render(report: &Report, json: bool) -> Result<bool> {
    if super::machine::enabled() {
        super::machine::outcome(if report.successful {
            "completed"
        } else {
            "incomplete"
        });
        super::machine::report("recovery", &report.document)?;
        return Ok(report.successful);
    }
    if json {
        serde_json::to_writer_pretty(std::io::stdout().lock(), &report.document)?;
        println!();
    } else {
        let status = report.document["status"].as_str().unwrap_or_default();
        match report.kind {
            Kind::Ordinary => {
                println!("Recovery: {status}. No phone operations replayed.");
                if !report.document["reason"].is_null() {
                    println!("{}", serde_json::to_string(&report.document["reason"])?);
                }
            }
            Kind::Batch => {
                let kind = report.document["kind"].as_str().unwrap_or_default();
                match &report.document["attempt"] {
                    serde_json::Value::Null => println!(
                        "{kind} batch {}: {status}. No operation was sent again.",
                        report.document["batch"]
                    ),
                    attempt => {
                        println!("{kind} attempt {attempt}: {status}. No operation was sent again.")
                    }
                }
                if status == "abandoned" {
                    println!(
                        "The phone took its metadata but its cover never arrived; the next sync or push of its source sends the cover again."
                    );
                }
                let compensation = &report.document["compensation"];
                if !compensation.is_null() {
                    println!(
                        "Removal batch {} took out {} rows without audio: {}.",
                        compensation["removal_batch"],
                        compensation["removed"],
                        compensation["status"].as_str().unwrap_or_default()
                    );
                }
            }
        }
    }
    if report.kind == Kind::Batch {
        std::io::stdout().flush()?;
    }
    Ok(report.successful)
}
