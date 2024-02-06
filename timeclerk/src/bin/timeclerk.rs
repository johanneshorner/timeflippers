use anyhow::{bail, format_err};
use chrono::{offset::Local, DateTime, NaiveDate, Utc};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use timeflippers::{
    timeflip::{Entry, TimeFlip},
    view, BluetoothSession, Config, Facet,
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process, select, signal,
};

async fn read_config(path: impl AsRef<Path>) -> anyhow::Result<Config> {
    let toml = fs::read_to_string(path).await?;
    let config: Config = toml::from_str(&toml)?;
    Ok(config)
}

fn facet_name(facet: &Facet, config: &Config) -> String {
    config.sides[facet.index_zero()]
        .name
        .clone()
        .unwrap_or(facet.to_string())
}

async fn load_history(history_file: impl AsRef<Path>) -> anyhow::Result<Vec<EntryEdit>> {
    match fs::read_to_string(history_file).await {
        Ok(s) => {
            let entries: Vec<EntryEdit> = serde_yaml::from_str(&s)?;
            Ok(entries)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}

async fn append_history(history_file: &PathBuf, entries: &[EntryEdit]) -> anyhow::Result<()> {
    let content = serde_yaml::to_string(&entries)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_file)
        .await?;
    file.write(content.as_bytes()).await?;
    Ok(())
}

/// Communicate with a TimeFlip2 cube.
///
/// Note: Use `bluetoothctl` to pair (and potentially connect) the TimeFlip2.
/// Currently, the TimeFlip2's password is expected to be the default value.
#[derive(Parser)]
#[clap(about)]
struct Options {
    #[arg(short, long, help = "path to the timeflip.toml file")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum HistoryStyle {
    Lines,
    Tabular,
    Summarized,
}

#[derive(Subcommand)]
enum HistoryCommand {
    List {
        #[arg(long, help = "read events from and write new events to file")]
        update: Option<PathBuf>,
        #[arg(
            long,
            help = "start reading with entry ID, latest event in `--update` takes precedence"
        )]
        start_with: Option<u32>,
        #[arg(long, help = "start displaying with entries after DATE (YYYY-MM-DD)")]
        since: Option<NaiveDate>,
        #[arg(long, help = "choose output style", default_value = "tabular")]
        style: HistoryStyle,
    },
    Edit {
        #[arg(
            long,
            help = "The editor to use",
            env = "EDITOR",
            default_value = "nano"
        )]
        editor: String,
        #[arg(long, help = "where to store the time entries")]
        history_file: Option<PathBuf>,
        #[arg(long, help = "start reading with entry ID")]
        start_id: Option<u32>,
        #[arg(long, help = "end id")]
        end_id: Option<u32>,
        // #[arg(long, help = "start displaying with entries after DATE (YYYY-MM-DD)")]
        // since: Option<NaiveDate>,
    },
}

/// An entry in an easy to edit format
#[derive(Debug, Serialize, Deserialize)]
struct EntryEdit {
    /// ID of the entry.
    pub id: u32,
    /// Active facet.
    pub facet: String,
    /// The time the dice was flipped.
    pub start_time: DateTime<Utc>,
    /// The time the dice was flipped to a different facet.
    pub end_time: DateTime<Utc>,
    /// Describes the time frame.
    pub description: String,
}

impl EntryEdit {
    fn from_entry_with_config(entry: &Entry, config: &Config) -> Self {
        let mut entry_edit: EntryEdit = entry.into();
        entry_edit.facet = facet_name(&entry.facet, config);
        entry_edit
    }
}

impl From<Entry> for EntryEdit {
    fn from(value: Entry) -> Self {
        Self {
            id: value.id,
            facet: value.facet.to_string(),
            start_time: value.time,
            end_time: value.time + value.duration,
            description: String::new(),
        }
    }
}

impl From<&Entry> for EntryEdit {
    fn from(value: &Entry) -> Self {
        Self {
            id: value.id,
            facet: value.facet.to_string(),
            start_time: value.time,
            end_time: value.time + value.duration,
            description: String::new(),
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Print logged TimeFlip events.
    #[command(subcommand)]
    History(HistoryCommand),
    GenerateCompletions {
        shell: clap_complete::Shell,
    },
}

impl Command {
    async fn run(&self, timeflip: &mut TimeFlip, config: Option<Config>) -> anyhow::Result<()> {
        use Command::*;
        match self {
            History(HistoryCommand::List {
                update: update_file,
                start_with,
                style,
                since,
            }) => {
                let config = config.ok_or(format_err!("config is mandatory for this command"))?;

                let (start_with, mut entries) = if let Some(file) = update_file {
                    match fs::read_to_string(file).await {
                        Ok(s) => {
                            let mut entries: Vec<Entry> = serde_json::from_str(&s)?;
                            entries.sort_by(|a, b| a.id.cmp(&b.id));
                            (
                                start_with
                                    .or_else(|| entries.last().map(|e| e.id))
                                    .unwrap_or(0),
                                entries,
                            )
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            (start_with.unwrap_or(0), vec![])
                        }
                        Err(e) => return Err(e.into()),
                    }
                } else {
                    (start_with.unwrap_or(0), vec![])
                };

                let mut update = timeflip.read_history_since(start_with).await?;

                let new_ids = update.iter().map(|e| e.id).collect::<Vec<_>>();
                entries.retain(|entry| !new_ids.contains(&entry.id));
                entries.append(&mut update);

                if let Some(file) = update_file {
                    match serde_json::to_vec(&entries) {
                        Ok(json) => {
                            if let Err(e) = fs::write(file, json).await {
                                eprintln!("cannot update entries file {}: {e}", file.display());
                            }
                        }
                        Err(e) => eprintln!("cannot update entries file {}: {e}", file.display()),
                    }
                }

                let history = view::History::new(entries, config);
                let filtered = if let Some(since) = since {
                    let date = DateTime::<Local>::from_local(
                        since.and_hms_opt(0, 0, 0).expect("is a valid time"),
                        *Local::now().offset(),
                    );

                    history.since(date.into())
                } else {
                    history.all()
                };
                use HistoryStyle::*;
                match style {
                    Lines => println!("{}", filtered),
                    Tabular => println!("{}", filtered.table_by_day()),
                    Summarized => println!("{}", filtered.summarized()),
                }
            }
            History(HistoryCommand::Edit {
                editor,
                history_file,
                start_id,
                end_id,
                ..
            }) => {
                let config = config.ok_or(format_err!("config is mandatory for this command"))?;

                let history_file_path = if let Some(path) = history_file {
                    path.to_owned()
                } else {
                    let history_file_path = dirs::data_local_dir()
                        .expect("a config directory to exist")
                        .join("timeclerk/persist.yaml");
                    if !history_file_path.exists() {
                        fs::create_dir_all(
                            history_file_path
                                .parent()
                                .expect("this path to have a parent, because we just created it"),
                        )
                        .await?;
                    }
                    history_file_path
                };
                let history = load_history(&history_file_path).await?;

                let start_id = start_id.unwrap_or_else(|| {
                    if let Some(entry) = history.last() {
                        entry.id + 1
                    } else {
                        1
                    }
                });

                let update = timeflip.read_history_since(start_id).await?;
                let entries: Vec<EntryEdit> = update
                    .iter()
                    .map(|e| EntryEdit::from_entry_with_config(e, &config))
                    .collect();
                let content = serde_yaml::to_string(&entries)?;

                // TODO: create with uuid as file name
                let temp_file_path = dirs::cache_dir()
                    .expect("a cache dir to exist")
                    .join("timeclerk/edit.yaml");
                if !temp_file_path.exists() {
                    fs::create_dir_all(
                        temp_file_path
                            .parent()
                            .expect("this path to have a parent, because we just created it"),
                    )
                    .await?;
                }
                let mut temp_file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&temp_file_path)
                    .await?;
                temp_file.write(content.as_bytes()).await?;

                process::Command::new(editor)
                    .arg(&temp_file_path)
                    .status()
                    .await?;
                tokio::time::sleep(Duration::from_secs(5)).await;

                // For some reason the content buffer is empty after the read call
                let mut content = String::new();
                temp_file.sync_data().await?;
                temp_file.seek(io::SeekFrom::Start(0)).await?;
                let bytes_read = temp_file.read_to_string(&mut content).await?;
                println!("{bytes_read}");
                let new_entries: Vec<EntryEdit> = serde_yaml::from_str(&content)?;
                println!("{:?}", new_entries.last().unwrap());
                // history.extend(new_entries.into_iter());
                // TODO processing
                append_history(&history_file_path, &new_entries).await?
            }
            GenerateCompletions { shell } => {
                clap_complete::generate(
                    *shell,
                    &mut Options::command(),
                    "timeflip",
                    &mut io::stdout(),
                );
            }
        }
        Ok(())
    }
}

// #[derive(Args)]
// struct HistoryArgs {
//
// }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let opt = Options::parse();
    let config_path = if opt.config.is_some() {
        opt.config
    } else {
        let path = dirs::config_dir()
            .expect("a config directory to exist")
            .join("timeflip/timeflip.toml");

        if path.exists() {
            Some(path)
        } else {
            None
        }
    };
    let config = if let Some(path) = config_path {
        Some(read_config(path).await?)
    } else {
        None
    };

    let (mut bg_task, session) = BluetoothSession::new().await?;

    let mut timeflip =
        TimeFlip::connect(&session, config.as_ref().map(|c| c.password.clone())).await?;
    log::info!("connected");

    select! {
        _ = signal::ctrl_c() => {
            log::info!("shutting down");
        }
        res = &mut bg_task => {
            if let Err(e) =res {
                log::error!("bluetooth session background task exited with error: {e}");
            }
        }
        res = opt.cmd.run(&mut timeflip, config) => {
            res?;
        }
    }

    Ok(())
}
