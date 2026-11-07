// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::project::validate_name;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Traffic {
    pub revision: String,
    pub percent: i32,
}

pub fn traffic(value: &str, current: &str) -> Result<Vec<Traffic>> {
    if value.trim().is_empty() {
        return Ok(vec![Traffic {
            revision: current.to_owned(),
            percent: 100,
        }]);
    }
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    for item in value.split(',') {
        let (name, percent) = item
            .trim()
            .split_once('=')
            .context("traffic must use revision=percent")?;
        let revision = if name.trim() == "current" {
            current
        } else {
            name.trim()
        };
        validate_name(revision)?;
        ensure!(
            seen.insert(revision.to_owned()),
            "traffic revision {revision:?} is repeated"
        );
        let percent: i32 = percent
            .trim()
            .parse()
            .context("traffic percentage must be an integer")?;
        ensure!(
            (0..=100).contains(&percent),
            "traffic percentage must be between 0 and 100"
        );
        targets.push(Traffic {
            revision: revision.to_owned(),
            percent,
        });
    }
    let total: i64 = targets.iter().map(|target| i64::from(target.percent)).sum();
    if total != 100 {
        bail!("traffic percentages total {total}, want 100");
    }
    Ok(targets)
}
