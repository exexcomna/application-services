// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    feature_utils,
    value_utils::{
        prepare_experiment, prepare_rollout, try_extract_data_list, try_find_experiment, CliUtils,
    },
    AppCommand, ExperimentListSource, ExperimentSource, LaunchableApp, NimbusApp,
};
use anyhow::Result;
use console::Term;
use serde_json::{json, Value};
use std::{path::PathBuf, process::Command};

pub(crate) fn process_cmd(cmd: &AppCommand) -> Result<bool> {
    let status = match cmd {
        AppCommand::ApplyFile {
            app,
            list,
            preserve_nimbus_db,
        } => app.apply_list(list, preserve_nimbus_db)?,
        AppCommand::CaptureLogs { app, file } => app.capture_logs(file)?,
        AppCommand::Enroll {
            app,
            params,
            experiment,
            rollouts,
            branch,
            preserve_targeting,
            preserve_bucketing,
            preserve_nimbus_db,
            ..
        } => app.enroll(
            params,
            experiment,
            rollouts,
            branch,
            preserve_targeting,
            preserve_bucketing,
            preserve_nimbus_db,
        )?,
        AppCommand::FetchList { params, list, file } => params.fetch_list(list, file)?,
        AppCommand::FetchRecipes {
            params,
            recipes,
            file,
        } => params.fetch_recipes(recipes, file)?,
        AppCommand::Kill { app } => app.kill_app()?,
        AppCommand::List { params, list } => params.list(list)?,
        AppCommand::LogState { app } => app.log_state()?,
        AppCommand::Reset { app } => app.reset_app()?,
        AppCommand::TailLogs { app } => app.tail_logs()?,
        AppCommand::Unenroll { app } => app.unenroll_all()?,
    };

    Ok(status)
}

fn prompt(term: &Term, command: &str) -> Result<()> {
    let prompt = term.style().cyan();
    let style = term.style().yellow();
    term.write_line(&format!(
        "{} {}",
        prompt.apply_to("$"),
        style.apply_to(command)
    ))?;
    Ok(())
}

impl LaunchableApp {
    fn exe(&self) -> Result<Command> {
        Ok(match self {
            Self::Android { device_id, .. } => {
                let adb_name = if std::env::consts::OS != "windows" {
                    "adb"
                } else {
                    "adb.exe"
                };
                let adb = std::env::var("ADB_PATH").unwrap_or_else(|_| adb_name.to_string());
                let mut cmd = Command::new(adb);
                if let Some(id) = device_id {
                    cmd.args(["-s", id]);
                }
                cmd
            }
            Self::Ios { .. } => {
                if std::env::consts::OS != "macos" {
                    panic!("Cannot run commands for iOS on anything except macOS");
                }
                let xcrun = std::env::var("XCRUN_PATH").unwrap_or_else(|_| "xcrun".to_string());
                let mut cmd = Command::new(xcrun);
                cmd.arg("simctl");
                cmd
            }
        })
    }

    fn kill_app(&self) -> Result<bool> {
        Ok(match self {
            Self::Android { package_name, .. } => self
                .exe()?
                .arg("shell")
                .arg(format!("am force-stop {}", package_name))
                .spawn()?
                .wait()?
                .success(),
            Self::Ios { app_id, device_id } => {
                let _ = self
                    .exe()?
                    .args(["terminate", device_id, app_id])
                    .output()?;
                true
            }
        })
    }

    fn unenroll_all(&self) -> Result<bool> {
        let payload = json! {{ "data": [] }};
        self.start_app(false, Some(&payload), true)
    }

    fn reset_app(&self) -> Result<bool> {
        Ok(match self {
            Self::Android { package_name, .. } => self
                .exe()?
                .arg("shell")
                .arg(format!("pm clear {}", package_name))
                .spawn()?
                .wait()?
                .success(),
            Self::Ios { app_id, device_id } => {
                self.exe()?
                    .args(["privacy", device_id, "reset", "all", app_id])
                    .status()?;
                let data = self.ios_app_container("data")?;
                let groups = self.ios_app_container("groups")?;
                self.ios_reset(data, groups)?;
                true
            }
        })
    }

    fn tail_logs(&self) -> Result<bool> {
        let term = Term::stdout();
        let _ = term.clear_screen();
        Ok(match self {
            Self::Android { .. } => {
                let mut args = logcat_args();
                args.append(&mut vec!["-v", "color"]);
                prompt(&term, &format!("adb {}", args.join(" ")))?;
                self.exe()?.args(args).spawn()?.wait()?.success()
            }
            Self::Ios { .. } => {
                prompt(
                    &term,
                    &format!("{} | xargs tail -f", self.ios_log_file_command()),
                )?;
                let log = self.ios_log_file()?;

                Command::new("tail")
                    .arg("-f")
                    .arg(log.as_path().to_str().unwrap())
                    .spawn()?
                    .wait()?
                    .success()
            }
        })
    }

    fn capture_logs(&self, file: &PathBuf) -> Result<bool> {
        let term = Term::stdout();
        Ok(match self {
            Self::Android { .. } => {
                let mut args = logcat_args();
                args.append(&mut vec!["-d"]);
                prompt(
                    &term,
                    &format!(
                        "adb {} > {}",
                        args.join(" "),
                        file.as_path().to_str().unwrap()
                    ),
                )?;
                let output = self.exe()?.args(args).output()?;
                std::fs::write(file, String::from_utf8_lossy(&output.stdout).to_string())?;
                true
            }

            Self::Ios { .. } => {
                let log = self.ios_log_file()?;
                prompt(
                    &term,
                    &format!(
                        "{} | xargs -J %log_file% cp %log_file% {}",
                        self.ios_log_file_command(),
                        file.as_path().to_str().unwrap()
                    ),
                )?;
                std::fs::copy(log, file)?;
                true
            }
        })
    }

    fn ios_log_file(&self) -> Result<PathBuf> {
        let data = self.ios_app_container("data")?;
        let mut files = glob::glob(&format!("{}/**/*.log", data))?;
        let log = files.next();
        Ok(log.ok_or_else(|| {
            anyhow::Error::msg(
                "Logs are not available before the app is started for the first time",
            )
        })??)
    }

    fn ios_log_file_command(&self) -> String {
        if let Self::Ios { device_id, app_id } = self {
            format!(
                "find $(xcrun simctl get_app_container {0} {1} data) -name \\*.log",
                device_id, app_id
            )
        } else {
            unreachable!()
        }
    }

    fn log_state(&self) -> Result<bool> {
        self.start_app(false, None, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn enroll(
        &self,
        params: &NimbusApp,
        experiment: &ExperimentSource,
        rollouts: &Vec<ExperimentSource>,
        branch: &str,
        preserve_targeting: &bool,
        preserve_bucketing: &bool,
        preserve_nimbus_db: &bool,
    ) -> Result<bool> {
        let term = Term::stdout();

        let experiment = Value::try_from(experiment)?;
        let slug = experiment.get_str("slug")?.to_string();

        let mut recipes = vec![prepare_experiment(
            &experiment,
            params,
            branch,
            *preserve_targeting,
            *preserve_bucketing,
        )?];
        prompt(
            &term,
            &format!("# Enrolling in the '{0}' branch of '{1}'", branch, &slug),
        )?;

        for r in rollouts {
            let rollout = Value::try_from(r)?;
            let slug = rollout.get_str("slug")?.to_string();
            recipes.push(prepare_rollout(
                &rollout,
                params,
                *preserve_targeting,
                *preserve_bucketing,
            )?);
            prompt(&term, &format!("# Enrolling into the '{0}' rollout", &slug))?;
        }

        let payload = json! {{ "data": recipes }};
        self.start_app(!preserve_nimbus_db, Some(&payload), true)
    }

    fn apply_list(&self, list: &ExperimentListSource, preserve_nimbus_db: &bool) -> Result<bool> {
        let value: Value = list.try_into()?;

        self.start_app(!preserve_nimbus_db, Some(&value), true)
    }

    fn ios_app_container(&self, container: &str) -> Result<String> {
        if let Self::Ios { app_id, device_id } = self {
            // We need to get the app container directories, and delete them.
            let output = self
                .exe()?
                .args(["get_app_container", device_id, app_id, container])
                .output()
                .expect("Expected an app-container from the simulator");
            let string = String::from_utf8_lossy(&output.stdout).to_string();
            Ok(string.trim().to_string())
        } else {
            unreachable!()
        }
    }

    fn ios_reset(&self, data_dir: String, groups_string: String) -> Result<bool> {
        let term = Term::stdout();
        prompt(&term, "# Resetting the app")?;
        if !data_dir.is_empty() {
            prompt(&term, &format!("rm -Rf {}/* 2>/dev/null", data_dir))?;
            let _ = std::fs::remove_dir_all(&data_dir);
            let _ = std::fs::create_dir_all(&data_dir);
        }
        let lines = groups_string.split('\n');

        for line in lines {
            let words = line.splitn(2, '\t').collect::<Vec<_>>();
            if let [_, dir] = words.as_slice() {
                if !dir.is_empty() {
                    prompt(&term, &format!("rm -Rf {}/* 2>/dev/null", dir))?;
                    let _ = std::fs::remove_dir_all(dir);
                    let _ = std::fs::create_dir_all(dir);
                }
            }
        }
        Ok(true)
    }

    fn start_app(&self, reset_db: bool, payload: Option<&Value>, log_state: bool) -> Result<bool> {
        Ok(match self {
            Self::Android { .. } => self
                .android_start(reset_db, payload, log_state)?
                .spawn()?
                .wait()?
                .success(),
            Self::Ios { .. } => self
                .ios_start(reset_db, payload, log_state)?
                .spawn()?
                .wait()?
                .success(),
        })
    }

    fn android_start(
        &self,
        reset_db: bool,
        json: Option<&Value>,
        log_state: bool,
    ) -> Result<Command> {
        if let Self::Android {
            package_name,
            activity_name,
            ..
        } = self
        {
            let reset_db_arg = if reset_db {
                Some("--ez reset-db true".to_string())
            } else {
                None
            };
            let experiments_arg = if let Some(s) = json {
                let json = s.to_string().replace('\'', "&apos;");
                Some(format!("--es experiments '{}'", json))
            } else {
                None
            };
            let log_state_arg = if log_state {
                Some("--ez log-state true".to_string())
            } else {
                None
            };

            let args = vec![reset_db_arg, experiments_arg, log_state_arg]
                .iter()
                .flatten()
                .map(String::clone)
                .collect::<Vec<_>>();

            let mut cmd = self.exe()?;
            // TODO add adb pass through args for debugger, wait for debugger etc.
            let sh = format!(
                r#"am start -n {}/{} \
        -a android.intent.action.MAIN \
        -c android.intent.category.LAUNCHER \
        --esn nimbus-cli \
        --ei version 1 \
        {}"#,
                package_name,
                activity_name,
                args.join(" \\\n        "),
            );
            cmd.arg("shell").arg(&sh);
            let term = Term::stdout();
            prompt(&term, &format!("adb shell \"{}\"", sh))?;
            Ok(cmd)
        } else {
            unreachable!();
        }
    }

    fn ios_start(&self, reset_db: bool, json: Option<&Value>, log_state: bool) -> Result<Command> {
        if let Self::Ios { app_id, device_id } = self {
            let mut cmd = self.exe()?;
            cmd.args(["launch", device_id, app_id])
                .arg("--nimbus-cli")
                .args(["--version", "1"]);

            let reset_db_arg = if reset_db {
                let arg = "--reset-db";
                cmd.arg(arg);
                Some(arg.to_string())
            } else {
                None
            };
            let experiments_arg = if let Some(s) = json {
                let json = s.to_string().replace('\'', "&apos;");
                let arg = "--experiments";
                let args = [arg, &json];
                cmd.args(args);
                Some(format!("{} '{}'", arg, json))
            } else {
                None
            };
            let log_state_arg = if log_state {
                let arg = "--log-state";
                cmd.arg(arg);
                Some(arg.to_string())
            } else {
                None
            };

            let args = vec![reset_db_arg, experiments_arg, log_state_arg]
                .iter()
                .flatten()
                .map(String::clone)
                .collect::<Vec<_>>();

            let sh = format!(
                r#"xcrun simctl launch {} {} \
        --nimbus-cli \
        --version 1 \
        {}"#,
                device_id,
                app_id,
                args.join(" \\\n        "),
            );
            let term = Term::stdout();
            prompt(&term, &sh)?;
            Ok(cmd)
        } else {
            unreachable!()
        }
    }
}

fn logcat_args<'a>() -> Vec<&'a str> {
    vec!["logcat", "-b", "main"]
}

impl TryFrom<&ExperimentSource> for Value {
    type Error = anyhow::Error;

    fn try_from(value: &ExperimentSource) -> Result<Value> {
        Ok(match value {
            ExperimentSource::FromList { slug, list } => {
                let value = Value::try_from(list)?;
                try_find_experiment(&value, slug)?
            }
            ExperimentSource::FromFeatureFiles {
                app,
                feature_id,
                files,
            } => feature_utils::create_experiment(app, feature_id, files)?,
        })
    }
}

impl TryFrom<&ExperimentListSource> for Value {
    type Error = anyhow::Error;

    fn try_from(value: &ExperimentListSource) -> Result<Value> {
        Ok(match value {
            ExperimentListSource::FromRemoteSettings {
                endpoint,
                is_preview,
            } => {
                use remote_settings::{Client, RemoteSettingsConfig};
                viaduct_reqwest::use_reqwest_backend();
                let collection_name = if *is_preview {
                    "nimbus-preview".to_string()
                } else {
                    "nimbus-mobile-experiments".to_string()
                };
                let config = RemoteSettingsConfig {
                    server_url: Some(endpoint.clone()),
                    bucket_name: None,
                    collection_name,
                };
                let client = Client::new(config)?;

                let response = client.get_records_raw()?;
                response.json::<Value>()?
            }
            ExperimentListSource::FromFile { file } => {
                let string = std::fs::read_to_string(file)?;
                serde_json::from_str(&string)?
            }
        })
    }
}

impl NimbusApp {
    fn fetch_list(&self, list: &ExperimentListSource, file: &PathBuf) -> Result<bool> {
        let value: Value = list.try_into()?;
        let array = try_extract_data_list(&value)?;
        let mut data = Vec::new();

        for exp in array {
            let app_name = exp.get_str("appName")?;
            if app_name != self.app_name {
                continue;
            }

            data.push(exp);
        }
        self.write_experiments_to_file(&data, file)?;
        Ok(true)
    }

    fn fetch_recipes(&self, recipes: &Vec<ExperimentSource>, file: &PathBuf) -> Result<bool> {
        let mut data = Vec::new();

        for exp in recipes {
            let exp: Value = exp.try_into()?;
            let app_name = exp.get_str("appName")?;
            if app_name != self.app_name {
                continue;
            }

            data.push(exp);
        }

        self.write_experiments_to_file(&data, file)?;
        Ok(true)
    }

    fn write_experiments_to_file(&self, data: &Vec<Value>, file: &PathBuf) -> Result<()> {
        let contents = json!({
            "data": data,
        });
        std::fs::write(file, serde_json::to_string_pretty(&contents)?)?;
        Ok(())
    }

    fn list(&self, list: &ExperimentListSource) -> Result<bool> {
        let value: Value = list.try_into()?;
        let array = try_extract_data_list(&value)?;
        let term = Term::stdout();
        let style = term.style().italic().underlined();
        term.write_line(&format!(
            "{0: <66}|{1: <31}|{2: <20}",
            style.apply_to("Experiment slug"),
            style.apply_to(" Features"),
            style.apply_to(" Branches")
        ))?;
        for exp in array {
            let slug = exp.get_str("slug")?;
            let app_name = exp.get_str("appName")?;
            if app_name != self.app_name {
                continue;
            }
            let features: Vec<_> = exp
                .get_array("featureIds")?
                .iter()
                .flat_map(|f| f.as_str())
                .collect();
            let branches: Vec<_> = exp
                .get_array("branches")?
                .iter()
                .flat_map(|b| {
                    b.get("slug")
                        .expect("Expecting a branch with a slug")
                        .as_str()
                })
                .collect();

            term.write_line(&format!(
                " {0: <65}| {1: <30}| {2}",
                slug,
                features.join(", "),
                branches.join(", ")
            ))?;
        }
        Ok(true)
    }
}
