//! `lessons`: a personal knowledge base of problems, their fixes, and the attempts that failed.

mod db;
mod journal;
mod query;
mod store;

use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::db::{Domain, OpenMode};
use crate::store::{FoundLesson, NewLesson};

#[derive(Parser)]
#[command(
    name = "lessons",
    version,
    about = "Record and search lessons learned: problems, fixes, and what did not work"
)]
struct Cli {
    /// Log each step to stderr (same as setting LESSONS_LOG=1)
    #[arg(long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the DB files and their schema
    Init {
        #[arg(long, value_enum, default_value_t = DomainChoice::All)]
        domain: DomainChoice,
    },
    /// Record a new lesson
    Add(AddArgs),
    /// Print one lesson, looked up in every DB file
    Show {
        id: String,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum DomainChoice {
    Work,
    Personal,
    Shared,
    All,
}

impl DomainChoice {
    fn domains(self) -> Vec<Domain> {
        match self {
            DomainChoice::Work => vec![Domain::Work],
            DomainChoice::Personal => vec![Domain::Personal],
            DomainChoice::Shared => vec![Domain::Shared],
            DomainChoice::All => Domain::ALL.to_vec(),
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Args)]
struct AddArgs {
    /// The DB file to write. Required: the domain is never guessed
    #[arg(long, value_enum)]
    to: Domain,

    /// Read one lesson as JSON from stdin; the only accepted value is `-`
    #[arg(long, value_name = "-", conflicts_with_all = ["edit", "fields"])]
    json: Option<String>,

    /// Write the lesson in $VISUAL or $EDITOR, starting from a template
    #[arg(long, conflicts_with = "fields")]
    edit: bool,

    #[command(flatten)]
    fields: FieldArgs,
}

/// The lesson fields as flags, shared by `add` (and `update` later).
#[derive(Args)]
#[group(id = "fields", multiple = true)]
struct FieldArgs {
    /// One-line summary
    #[arg(long)]
    title: Option<String>,
    /// What went wrong; paste the exact error text when there is one
    #[arg(long)]
    problem: Option<String>,
    /// Why it went wrong (may be filled in later)
    #[arg(long)]
    root_cause: Option<String>,
    /// What worked
    #[arg(long)]
    resolution: Option<String>,
    /// What did NOT work
    #[arg(long = "failed")]
    failed_tries: Option<String>,
    /// Where: repo name, 'car', 'home network', ...
    #[arg(long)]
    context: Option<String>,
    /// Space-separated tags
    #[arg(long)]
    tags: Option<String>,
    /// Versions or dates that let a reader judge staleness
    #[arg(long = "env")]
    env_versions: Option<String>,
}

impl FieldArgs {
    fn into_new_lesson(self) -> Result<NewLesson> {
        match (self.title, self.problem) {
            (Some(title), Some(problem)) => Ok(NewLesson {
                title,
                problem,
                context: self.context,
                root_cause: self.root_cause,
                resolution: self.resolution,
                failed_tries: self.failed_tries,
                tags: self.tags,
                env_versions: self.env_versions,
            }),
            (None, _) | (_, None) => {
                bail!("--title and --problem are required (or use `--json -` or `--edit`)")
            }
        }
    }
}

fn main() -> Result<()> {
    let cli: Cli = Cli::parse();
    journal::init(cli.verbose);
    let lessons_home: PathBuf = db::lessons_home()?;
    journal!("start: LESSONS_HOME is {}", lessons_home.display());

    match cli.command {
        Command::Init { domain } => run_init(&lessons_home, domain),
        Command::Add(add_args) => run_add(&lessons_home, add_args),
        Command::Show { id, format } => run_show(&lessons_home, &id, format),
    }
}

fn run_init(lessons_home: &Path, domain_choice: DomainChoice) -> Result<()> {
    domain_choice.domains().into_iter().try_for_each(|domain| {
        let db_path: PathBuf = domain.file_path(lessons_home);
        let already_existed: bool = db_path.exists();
        db::open(&db_path, OpenMode::Create)?; // opening migrates, so an existing file is brought up to date too
        println!(
            "{} {:<8} {}",
            if already_existed {
                "ready  "
            } else {
                "created"
            },
            domain.as_str(),
            db_path.display()
        );
        Ok(())
    })
}

fn run_add(lessons_home: &Path, add_args: AddArgs) -> Result<()> {
    // Open the target first, so a missing file fails before anyone spends time typing in an editor.
    let conn = db::open_target(lessons_home, add_args.to)?;

    let (new_lesson, edited_template): (NewLesson, Option<PathBuf>) =
        match (add_args.json.as_deref(), add_args.edit) {
            (Some("-"), _) => (
                serde_json::from_reader(std::io::stdin().lock())
                    .context("cannot parse the lesson JSON on stdin")?,
                None,
            ),
            (Some(other_source), _) => {
                bail!("--json reads only stdin: pass `--json -`, not `--json {other_source}`")
            }
            (None, true) => new_lesson_from_editor()
                .map(|(new_lesson, template_path)| (new_lesson, Some(template_path)))?,
            (None, false) => (add_args.fields.into_new_lesson()?, None),
        };

    // On failure the edited template stays on disk, so typed text is never lost.
    let lesson_id: String =
        store::insert(&conn, &new_lesson).map_err(|insert_error| match &edited_template {
            Some(template_path) => insert_error.context(format!(
                "your edited text is kept at {}",
                template_path.display()
            )),
            None => insert_error,
        })?;
    if let Some(template_path) = &edited_template {
        std::fs::remove_file(template_path).ok(); // a leftover file in the temp dir is harmless, so a failed delete is ignored
    }
    println!("{lesson_id}");
    Ok(())
}

const EDIT_TEMPLATE: &str = r#"# New lesson. Fill in at least title and problem, then save and quit.
# Leave a field as "" to skip it. Triple-quoted values may span several lines.
title = ""
problem = """
"""
context = ""
root_cause = ""
resolution = """
"""
failed_tries = """
"""
tags = ""
env_versions = ""
"#;

/// Opens the template in the user's editor and parses the result. Returns the template's path,
/// which the caller deletes only once the lesson is stored.
fn new_lesson_from_editor() -> Result<(NewLesson, PathBuf)> {
    let template_path: PathBuf =
        std::env::temp_dir().join(format!("lessons-{}.toml", ulid::Ulid::generate()));
    std::fs::write(&template_path, EDIT_TEMPLATE)
        .with_context(|| format!("cannot write the template {}", template_path.display()))?;

    let fn_non_empty_env = |variable_name: &str| {
        std::env::var(variable_name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let editor_command: String = fn_non_empty_env("VISUAL")
        .or_else(|| fn_non_empty_env("EDITOR"))
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "vi" }.to_string());
    // $EDITOR may carry arguments ("code --wait"); quoted paths with spaces are not supported.
    let editor_words: Vec<&str> = editor_command.split_whitespace().collect();
    let (editor_program, editor_args) = editor_words
        .split_first()
        .context("the editor command is empty")?;
    journal!(
        "add --edit: running {editor_command} on {}",
        template_path.display()
    );

    let editor_status = Process::new(editor_program)
        .args(editor_args)
        .arg(&template_path)
        .status()
        .with_context(|| format!("cannot start the editor `{editor_command}`"))?;
    if !editor_status.success() {
        bail!(
            "the editor exited with {editor_status}; the template is kept at {}",
            template_path.display()
        );
    }

    let edited_text: String = std::fs::read_to_string(&template_path)?;
    let new_lesson: NewLesson = toml::from_str(&edited_text).with_context(|| {
        format!(
            "cannot parse {}; fix it and keep a copy, it is not deleted",
            template_path.display()
        )
    })?;
    Ok((new_lesson, template_path))
}

fn run_show(lessons_home: &Path, lesson_id: &str, format: OutputFormat) -> Result<()> {
    let opened = db::open_existing_domains(lessons_home, &Domain::ALL)?;
    let searched_names: Vec<&str> = opened
        .iter()
        .map(|(domain, _conn)| domain.as_str())
        .collect();
    let found: FoundLesson = store::find_in_domains(&opened, lesson_id)?.with_context(|| {
        if searched_names.is_empty() {
            format!(
                "no DB files under {}; run `lessons init`",
                lessons_home.display()
            )
        } else {
            format!(
                "no lesson with id {lesson_id} in {}",
                searched_names.join(", ")
            )
        }
    })?;
    match format {
        OutputFormat::Text => print!("{}", lesson_as_text(&found)),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&found)?),
    }
    Ok(())
}

/// Width of the label column in text output; continuation lines are indented to match.
const LABEL_WIDTH: usize = 14;

fn lesson_as_text(found: &FoundLesson) -> String {
    let lesson = &found.lesson;
    let continuation_indent: String = format!("\n  {}", " ".repeat(LABEL_WIDTH));
    let field_lines: String = [
        ("context", lesson.context.as_deref()),
        ("problem", Some(lesson.problem.as_str())),
        ("root cause", lesson.root_cause.as_deref()),
        ("resolution", lesson.resolution.as_deref()),
        ("failed", lesson.failed_tries.as_deref()),
        ("tags", lesson.tags.as_deref()),
        ("env", lesson.env_versions.as_deref()),
        ("superseded by", lesson.superseded_by.as_deref()),
    ]
    .iter()
    .filter_map(|(label, value)| {
        value.map(|text| {
            format!(
                "  {label:<LABEL_WIDTH$}{}\n",
                text.trim_end().replace('\n', &continuation_indent)
            )
        })
    })
    .collect();
    format!(
        "{} [{}] {}\n{field_lines}  {:<LABEL_WIDTH$}{} created, {} updated, {} hits\n",
        lesson.id,
        found.domain,
        lesson.title,
        "recorded",
        lesson.created_at,
        lesson.updated_at,
        lesson.hits
    )
}
