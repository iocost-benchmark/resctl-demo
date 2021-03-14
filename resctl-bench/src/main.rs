// Copyright (c) Facebook, Inc. and its affiliates.
use anyhow::{anyhow, bail, Context, Error, Result};
use log::{debug, error, info, warn};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::sync::{Arc, Mutex};
use util::*;

use resctl_bench_intf::{Args, Mode};

mod bench;
mod job;
mod progress;
mod run;
mod study;

use job::JobCtxs;
use run::RunCtx;

lazy_static::lazy_static! {
    pub static ref AGENT_BIN: String =
        find_bin("rd-agent", exe_dir().ok())
        .expect("can't find rd-agent")
        .to_str()
        .expect("non UTF-8 in rd-agent path")
        .to_string();
}

pub fn parse_json_value_or_dump<T>(value: serde_json::Value) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    const DUMP_PATH: &str = "/tmp/rb-debug-dump.json";

    match serde_json::from_value::<T>(value.clone()) {
        Ok(v) => Ok(v),
        Err(e) => {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(DUMP_PATH)
                .unwrap();
            f.write_all(serde_json::to_string_pretty(&value).unwrap().as_bytes())
                .unwrap();
            Err(Error::new(e)).with_context(|| format!("content dumped to {:?}", DUMP_PATH))
        }
    }
}

struct Program {
    args_file: JsonConfigFile<Args>,
    args_updated: bool,
    jobs: Arc<Mutex<JobCtxs>>,
}

impl Program {
    fn rd_agent_base_args(
        dir: &str,
        systemd_timeout: f64,
        dev: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut args = vec![
            "--dir".into(),
            dir.into(),
            "--bench-file".into(),
            Args::RB_BENCH_FILENAME.into(),
            "--force".into(),
            "--force-running".into(),
            "--systemd-timeout".into(),
            format!("{}", systemd_timeout),
        ];
        if dev.is_some() {
            args.push("--dev".into());
            args.push(dev.unwrap().into());
        }
        Ok(args)
    }

    fn clean_up_report_files(&self) -> Result<()> {
        let args = &self.args_file.data;
        let rep_1min_retention = args
            .rep_retention
            .max(rd_agent_intf::Args::default().rep_1min_retention);

        let mut cmd = Command::new(&*AGENT_BIN);
        cmd.args(&Program::rd_agent_base_args(
            &args.dir,
            args.systemd_timeout,
            args.dev.as_deref(),
        )?)
        .args(&["--linux-tar", "__SKIP__"])
        .args(&["--bypass", "--prepare"])
        .args(&["--rep-retention", &format!("{}", args.rep_retention)])
        .args(&["--rep-1min-retention", &format!("{}", rep_1min_retention)]);
        if args.clear_reports {
            cmd.arg("--reset");
        }

        let status = cmd.status()?;
        if !status.success() {
            bail!("failed to clean up rd-agent report files ({})", &status);
        }

        Ok(())
    }

    fn prep_base_bench(
        &self,
        scr_devname: &str,
        iocost_sys_save: &IoCostSysSave,
    ) -> Result<rd_agent_intf::BenchKnobs> {
        let args = &self.args_file.data;

        let (dev_model, dev_fwrev, dev_size) =
            devname_to_model_fwrev_size(&scr_devname).map_err(|e| {
                anyhow!(
                    "failed to resolve model/fwrev/size for {:?} ({})",
                    &scr_devname,
                    &e
                )
            })?;

        let demo_bench_path = args.demo_bench_path();

        let mut bench = match rd_agent_intf::BenchKnobs::load(&demo_bench_path) {
            Ok(v) => v,
            Err(e) => {
                match e.downcast_ref::<std::io::Error>() {
                    Some(e) if e.raw_os_error() == Some(libc::ENOENT) => (),
                    _ => warn!(
                        "Failed to load {:?} ({}), remove the file",
                        &demo_bench_path, &e
                    ),
                }
                Default::default()
            }
        };

        if bench.iocost_dev_model.len() > 0 && bench.iocost_dev_model != dev_model {
            bail!(
                "benchfile device model {:?} doesn't match detected {:?}",
                &bench.iocost_dev_model,
                &dev_model
            );
        }
        if bench.iocost_dev_fwrev.len() > 0 && bench.iocost_dev_fwrev != dev_fwrev {
            bail!(
                "benchfile device firmware revision {:?} doesn't match detected {:?}",
                &bench.iocost_dev_fwrev,
                &dev_fwrev
            );
        }
        if bench.iocost_dev_size > 0 && bench.iocost_dev_size != dev_size {
            bail!(
                "benchfile device size {} doesn't match detected {}",
                bench.iocost_dev_size,
                dev_size
            );
        }

        bench.iocost_dev_model = dev_model;
        bench.iocost_dev_fwrev = dev_fwrev;
        bench.iocost_dev_size = dev_size;

        if args.iocost_from_sys {
            if !iocost_sys_save.enable {
                bail!(
                    "--iocost-from-sys specified but iocost is disabled for {:?}",
                    &scr_devname
                );
            }
            bench.iocost_seq = 1;
            bench.iocost.model = iocost_sys_save.model.clone();
            bench.iocost.qos = iocost_sys_save.qos.clone();
            info!("Using iocost parameters from \"/sys/fs/cgroup/io.cost.model,qos\"");
        } else {
            info!("Using iocost parameters from {:?}", &demo_bench_path);
        }

        Ok(bench)
    }

    fn commit_args(&self) {
        // Everything parsed okay. Update the args file.
        if self.args_updated {
            if let Err(e) = Args::save_args(&self.args_file) {
                error!("Failed to update args file ({})", &e);
                panic!();
            }
        }
    }

    fn do_run(&mut self) {
        let mut base_bench = if self.args_file.data.study_rep_d.is_none() {
            // Use alternate bench file to avoid clobbering resctl-demo bench
            // results w/ e.g. fake_cpu_load ones.
            let scr_devname = match self.args_file.data.dev.as_ref() {
                Some(dev) => dev.clone(),
                None => {
                    let mut scr_path = PathBuf::from(&self.args_file.data.dir);
                    scr_path.push("scratch");
                    while !scr_path.exists() {
                        if !scr_path.pop() {
                            panic!("failed to find existing ancestor dir for scratch path");
                        }
                    }
                    path_to_devname(&scr_path.as_os_str().to_str().unwrap())
                        .expect("failed to resolve device for scratch path")
                        .into_string()
                        .unwrap()
                }
            };
            let scr_devnr = devname_to_devnr(&scr_devname)
                .expect("failed to resolve device number for scratch device");
            let iocost_sys_save =
                IoCostSysSave::read_from_sys(scr_devnr).expect("failed to read iocost.model,qos");

            match self.prep_base_bench(&scr_devname, &iocost_sys_save) {
                Ok(v) => v,
                Err(e) => {
                    error!("Failed to prepare bench files ({})", &e);
                    panic!();
                }
            }
        } else {
            // We aren't actually gonna run anything in study mode. Just use
            // dummy value.
            Default::default()
        };

        // Collect the pending jobs.
        let mut jobs = self.jobs.lock().unwrap();
        let mut pending = JobCtxs::default();
        let args = &self.args_file.data;
        for spec in args.job_specs.iter() {
            match jobs.parse_job_spec_and_link(spec) {
                Ok(new) => pending.vec.push(new),
                Err(e) => {
                    error!("{}: {}", spec, &e);
                    exit(1);
                }
            }
        }

        debug!("job_ctxs: nr_to_run={}\n{:#?}", pending.vec.len(), &pending);
        self.commit_args();

        if pending.vec.len() > 0 && !args.keep_reports {
            if let Err(e) = self.clean_up_report_files() {
                warn!("Failed to clean up report files ({})", &e);
            }
        }

        debug!(
            "job_ids: pending={} prev={}",
            &pending.format_ids(),
            jobs.format_ids()
        );

        // Run the benches and print out the results.
        drop(jobs);
        for jctx in pending.vec.into_iter() {
            let mut rctx = RunCtx::new(&args, &mut base_bench, self.jobs.clone());
            let name = format!("{}", &jctx.data.spec);
            if let Err(e) = rctx.run_jctx(jctx) {
                error!("{}: {:?}", &name, &e);
                panic!();
            }
        }
    }

    fn do_format(&mut self, mode: Mode) {
        let specs = &self.args_file.data.job_specs;
        let empty_props = vec![Default::default()];
        let mut to_format = vec![];
        let mut jctxs = JobCtxs::default();
        std::mem::swap(&mut jctxs, &mut self.jobs.lock().unwrap());

        if specs.len() == 0 {
            to_format = jctxs.vec.into_iter().map(|x| (x, &empty_props)).collect();
        } else {
            for spec in specs.iter() {
                let jctx = match jctxs.pop_matching_jctx(&spec) {
                    Some(v) => v,
                    None => {
                        error!("No matching result for {}", &spec);
                        exit(1);
                    }
                };

                let desc = jctx.bench.as_ref().unwrap().desc();
                if !desc.takes_format_props && spec.props[0].len() > 0 {
                    error!(
                        "Unknown properties specified for formatting {}",
                        &jctx.data.spec
                    );
                    exit(1);
                }
                if !desc.takes_format_propsets && spec.props.len() > 1 {
                    error!(
                        "Multiple property sets not supported for formatting {}",
                        &jctx.data.spec
                    );
                    exit(1);
                }
                to_format.push((jctx, &spec.props));
            }
        }

        for (jctx, props) in to_format.iter() {
            if let Err(e) = jctx.print(mode, props) {
                error!("Failed to format {}: {:#}", &jctx.data.spec, &e);
                panic!();
            }
        }

        self.commit_args();
    }

    fn main(mut self) {
        let args = &self.args_file.data;

        // Load existing result file into job_ctxs.
        if Path::new(&args.result).exists() {
            let mut jobs = self.jobs.lock().unwrap();
            *jobs = match JobCtxs::load_results(&args.result) {
                Ok(jctxs) => {
                    debug!("Loaded {} entries from result file", jctxs.vec.len());
                    jctxs
                }
                Err(e) => {
                    error!(
                        "Failed to load existing result file {:?} ({:#})",
                        &args.result, &e
                    );
                    panic!();
                }
            }
        }

        match args.mode {
            Mode::Run => self.do_run(),
            Mode::Format => self.do_format(Mode::Format),
            Mode::Summary => self.do_format(Mode::Summary),
        }
    }
}

fn main() {
    setup_prog_state();
    bench::init_benchs();

    let (args_file, args_updated) = Args::init_args_and_logging_nosave().unwrap_or_else(|e| {
        error!("Failed to process args file ({})", &e);
        panic!();
    });

    systemd::set_systemd_timeout(args_file.data.systemd_timeout);

    Program {
        args_file,
        args_updated,
        jobs: Arc::new(Mutex::new(JobCtxs::default())),
    }
    .main();
}
