// Copyright (c) 2023 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use anyhow::Result;

use crate::AGENT_POLICY;

#[cfg(feature = "agent-policy")]
pub async fn get_string_value(key: &str) -> Result<String> {
    let opa_data_path = format!("kata-agent/{key}");
    let mut policy = AGENT_POLICY.lock().await;
    Ok(policy.get_data(&opa_data_path).await?)
}
