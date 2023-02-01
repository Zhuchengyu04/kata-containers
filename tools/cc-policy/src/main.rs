// Copyright (c) Microsoft Corporation.
// Licensed under the Apache 2.0 license.

#[macro_use]
mod macros;
mod cri;
mod image;
mod kubernetes;
mod oci;
mod pod_yaml;
mod policy;

use kubernetes::KubeCtl;
use pod_yaml::*;
use policy::*;

use clap::Parser;
use std::fs::{read_to_string, File};
use std::io::prelude::*;
use std::path::PathBuf;

use anyhow::{bail, Result};

use serde::{Deserialize, Serialize};

#[derive(Parser)]
struct Cli {
    #[clap(short = 'i', long = "input")]
    input_yaml: Option<PathBuf>,
    #[clap(long = "image_ref")]
    image_ref: Option<String>,
    #[clap(short = 'o', long = "output")]
    output_yaml: Option<PathBuf>,
    #[clap(short = 'p', long = "policy")]
    output_policy: Option<PathBuf>,
    #[clap(long = "with_default_rules")]
    with_default_rules: bool,
    #[clap(short = 'v', long = "verbose")]
    verbose: bool,
}

fn get_policy_from_yaml(
    yaml: &serde_yaml::Value,
    with_default_rules: bool,
) -> Result<(String, String, String)> {
    let pod_yaml = PodYaml::from(yaml)?;

    let policy = CcPolicy::from_pod_yaml(&pod_yaml, with_default_rules)?;

    Ok((
        pod_yaml.kind.to_string(),
        policy.to_string(),
        policy.to_base64(),
    ))
}

fn create_and_inject_policy(
    path: &PathBuf,
    with_default_rules: bool,
) -> Result<(String, String, String)> {
    let yaml = read_to_string(path)?;
    let mut buffer = Vec::new();
    let mut ser = serde_yaml::Serializer::new(&mut buffer);
    let mut policy_list = Vec::new();
    let mut policy_base64_list = Vec::new();

    let yaml_from_dry_run = KubeCtl::get_yaml_with_dry_run_server(path)?;

    for doc in serde_yaml::Deserializer::from_str(yaml.as_str()) {
        let mut yaml = serde_yaml::Value::deserialize(doc)?;

        if let Ok((kind, policy, policy_base64)) =
            get_policy_from_yaml(&yaml_from_dry_run, with_default_rules)
        {
            patch_yaml(&mut yaml, &kind, &policy_base64)?;
            policy_list.push(policy.clone());
            policy_base64_list.push(policy_base64.clone());
        }

        yaml.serialize(&mut ser)?;
    }

    let yaml_with_policy = String::from_utf8_lossy(&buffer).to_string();

    let policy = policy_list.join("\n");
    let policy_base64 = policy_base64_list.join("\n");

    Ok((policy, policy_base64, yaml_with_policy))
}

fn create_policy_by_image_ref(
    image_ref: &str,
    with_default_rules: bool,
) -> Result<(String, String)> {
    let policy = CcPolicy::from_image_ref(image_ref, with_default_rules)?;

    Ok((policy.to_string(), policy.to_base64()))
}

fn write_to_file(data: &str, path: &PathBuf) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(data.as_bytes())?;

    println!("{} created.", path.display());

    Ok(())
}

fn main() -> Result<()> {
    let args = Cli::parse();

    if args.input_yaml == None && args.image_ref == None {
        bail!("Please specify either input_yaml or image_ref");
    }

    if args.input_yaml != None && args.image_ref != None {
        bail!("Cannot specify input_yaml and image_ref at the same time");
    }

    let policy;
    let policy_encoded;
    let mut patched_yaml = String::new();

    if let Some(input_yaml) = args.input_yaml {
        (policy, policy_encoded, patched_yaml) =
            create_and_inject_policy(&input_yaml, args.with_default_rules)?;
    } else if let Some(image_ref) = args.image_ref {
        (policy, policy_encoded) = create_policy_by_image_ref(&image_ref, args.with_default_rules)?;
    } else {
        unreachable!();
    }

    if args.verbose {
        println!("CcPolicy: {}", policy);
        println!("Base64 encoding: {}", policy_encoded);
        println!("Encoding size: {}", policy_encoded.len());
    }

    if let Some(output_policy) = args.output_policy {
        write_to_file(&policy, &output_policy)?;
    }

    if let Some(output_yaml) = args.output_yaml {
        write_to_file(&patched_yaml, &output_yaml)?;
    }

    Ok(())
}
