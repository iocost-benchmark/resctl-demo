// Copyright (c) Facebook, Inc. and its affiliates.
use super::{prepare_bin_file, Config};
use anyhow::Result;
use std::process::Command;
use util::*;

const MISC_BINS: [(&str, &[u8]); 4] = [
    (
        "iocost_coef_gen.py",
        include_bytes!("misc/iocost_coef_gen.py"),
    ),
    ("sideloader.py", include_bytes!("misc/sideloader.py")),
    ("biolatpcts.py", include_bytes!("misc/biolatpcts.py")),
    (
        "biolatpcts_wrapper.sh",
        include_bytes!("misc/biolatpcts_wrapper.sh"),
    ),
];

pub fn prepare_misc_bins(cfg: &Config) -> Result<()> {
    for (name, body) in &MISC_BINS {
        prepare_bin_file(&format!("{}/{}", &cfg.misc_bin_path, name), body)?;
    }

    if cfg.biolatpcts_bin.is_some() {
        run_command(
            Command::new(cfg.biolatpcts_bin.as_ref().unwrap())
                .arg(format!("{}:{}", cfg.scr_devnr.0, cfg.scr_devnr.1))
                .args(&["-i", "0"]),
            "is bcc working? https://github.com/iovisor/bcc",
        )?;
    }

    Ok(())
}
