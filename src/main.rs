//! `lessons`: a personal knowledge base of problems, their fixes, and the attempts that failed.

mod config;
mod db;
mod hook;
mod journal;
mod mcp;
mod query;
mod store;
mod sync;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::db::{Domain, OpenMode};
use crate::store::{FoundLesson, LessonPatch, NewLesson, SearchHit};

/// Default `--limit` of `list` and of `search`.
const LIST_DEFAULT_LIMIT: usize = 20;
const SEARCH_DEFAULT_LIMIT: usize = 10;

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
    /// List the newest lessons
    List {
        /// Comma-separated domains (work,personal,shared) or `all`
        #[arg(long = "in", value_name = "DOMAINS", default_value = "all", value_parser = parse_domain_list)]
        in_domains: DomainList,
        #[arg(long, default_value_t = LIST_DEFAULT_LIMIT)]
        limit: usize,
        /// Only lessons carrying this whole tag
        #[arg(long)]
        tag: Option<String>,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Change fields of a lesson; an empty value (--context "") clears that field
    Update {
        id: String,
        #[command(flatten)]
        fields: FieldArgs,
    },
    /// Record that a retrieved lesson actually helped
    Hit { id: String },
    /// Search lessons by keywords or pasted error text
    Search {
        /// Keywords or pasted error text; may start with `-`, as error text often does
        #[arg(allow_hyphen_values = true)]
        text: String,
        /// Comma-separated domains (work,personal,shared) or `all`; default from config.toml
        #[arg(long = "in", value_name = "DOMAINS", value_parser = parse_domain_list)]
        in_domains: Option<DomainList>,
        #[arg(long, default_value_t = SEARCH_DEFAULT_LIMIT)]
        limit: usize,
        /// Pass TEXT to FTS5 unchanged, as a query expression
        #[arg(long)]
        raw: bool,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Move a lesson to another DB file, keeping its id
    Move {
        id: String,
        #[arg(long, value_enum)]
        to: Domain,
    },
    /// Hide a lesson from search because a newer one replaces it
    Retire {
        id: String,
        /// The id of the lesson that replaces it
        #[arg(long)]
        by: String,
    },
    /// Sync the lessons with the git repository each [sync.<domain>] names; needs git on the PATH
    Sync {
        /// Only this domain (work, personal or shared); default: every configured domain. Deletes propagate only on a
        /// run that covers every domain of a remote
        #[arg(long)]
        domain: Option<Domain>,
    },
    /// Serve search_lessons and record_lesson to an MCP client over stdio
    Mcp,
    /// Entry points for the prompt hook; they never block a prompt
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },
}

#[derive(Subcommand)]
enum HookEvent {
    /// Print lessons relevant to the submitted prompt (reads the hook JSON on stdin)
    UserPromptSubmit,
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

/// The lesson fields as flags, shared by `add` and `update`. Every value may start with `-`: pasted
/// error text and command lines often do (`-o IdentityFile=none`), and clap would read them as options.
/// The cost: a flag-like word right after a field flag is taken as its value (`--failed --tags` stores "--tags").
#[derive(Args)]
#[group(id = "fields", multiple = true)]
struct FieldArgs {
    /// One-line summary
    #[arg(long, allow_hyphen_values = true)]
    title: Option<String>,
    /// What went wrong; paste the exact error text when there is one
    #[arg(long, allow_hyphen_values = true)]
    problem: Option<String>,
    /// Why it went wrong (may be filled in later)
    #[arg(long, allow_hyphen_values = true)]
    root_cause: Option<String>,
    /// What worked
    #[arg(long, allow_hyphen_values = true)]
    resolution: Option<String>,
    /// What did NOT work
    #[arg(long = "failed", allow_hyphen_values = true)]
    failed_tries: Option<String>,
    /// Where: repo name, 'car', 'home network', ...
    #[arg(long, allow_hyphen_values = true)]
    context: Option<String>,
    /// Space-separated tags
    #[arg(long, allow_hyphen_values = true)]
    tags: Option<String>,
    /// Versions or dates that let a reader judge staleness
    #[arg(long = "env", allow_hyphen_values = true)]
    env_versions: Option<String>,
}

impl FieldArgs {
    fn into_patch(self) -> LessonPatch {
        LessonPatch {
            title: self.title,
            problem: self.problem,
            context: self.context,
            root_cause: self.root_cause,
            resolution: self.resolution,
            failed_tries: self.failed_tries,
            tags: self.tags,
            env_versions: self.env_versions,
        }
    }

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

fn main() -> Result<ExitCode> {
    let cli: Cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // clap exits 2 on a usage error, and the agent reads exit 2 from the prompt hook as "block
        // the prompt": a mistyped hook command in the agent's settings would block every prompt. Report it
        // and let the prompt through instead.
        // The subcommand is the first argument that is not a flag (`lessons --verbose hook ...`).
        Err(parse_error)
            if std::env::args()
                .skip(1)
                .find(|arg| !arg.starts_with('-'))
                .as_deref()
                == Some("hook") =>
        {
            eprintln!("lessons hook: {parse_error}");
            return Ok(ExitCode::SUCCESS);
        }
        Err(parse_error) => parse_error.exit(),
    };
    journal::init(cli.verbose);
    // Resolved per command, so the hook can turn a missing home directory into a stderr line.
    let fn_lessons_home = || -> Result<PathBuf> {
        let lessons_home: PathBuf = db::lessons_home()?;
        journal!("start: LESSONS_HOME is {}", lessons_home.display());
        Ok(lessons_home)
    };

    // Every command but `sync` ends in success or in an error (exit 1); `sync` also has an exit code of its own.
    let fn_done = |result: Result<()>| result.map(|()| ExitCode::SUCCESS);
    // R13: a remote recorded public is warned about on stderr by every command but `init`, from one place and from the
    // state files alone (no network). The warning is a caution: nothing here can fail or change a command.
    if !matches!(cli.command, Command::Init { .. }) {
        warn_public_remotes(&fn_lessons_home);
    }
    match cli.command {
        Command::Init { domain } => fn_done(run_init(&fn_lessons_home()?, domain)),
        Command::Add(add_args) => fn_done(run_add(&fn_lessons_home()?, add_args)),
        Command::Show { id, format } => fn_done(run_show(&fn_lessons_home()?, &id, format)),
        Command::List {
            in_domains,
            limit,
            tag,
            format,
        } => fn_done(run_list(
            &fn_lessons_home()?,
            &in_domains.0,
            limit,
            tag.as_deref(),
            format,
        )),
        Command::Update { id, fields } => {
            fn_done(run_update(&fn_lessons_home()?, &id, fields.into_patch()))
        }
        Command::Hit { id } => fn_done(run_hit(&fn_lessons_home()?, &id)),
        Command::Search {
            text,
            in_domains,
            limit,
            raw,
            format,
        } => fn_done(run_search(
            &fn_lessons_home()?,
            &text,
            in_domains,
            limit,
            raw,
            format,
        )),
        Command::Retire { id, by } => fn_done(run_retire(&fn_lessons_home()?, &id, &by)),
        Command::Move { id, to } => fn_done(run_move(&fn_lessons_home()?, &id, to)),
        Command::Sync { domain } => run_sync(&fn_lessons_home()?, domain),
        Command::Mcp => fn_done(mcp::serve(
            &fn_lessons_home()?,
            std::io::stdin().lock(),
            std::io::stdout().lock(),
        )),
        Command::Hook { event } => {
            run_hook(event, fn_lessons_home);
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Prints the PUBLIC warning of every configured clone whose recorded visibility is public. A config that cannot be read
/// only goes to the journal: this must never make a command fail.
fn warn_public_remotes(fn_lessons_home: &impl Fn() -> Result<PathBuf>) {
    let warnings: Result<Vec<String>> = fn_lessons_home().and_then(|lessons_home| {
        let config: config::Config = config::load(&lessons_home)?;
        let clones = config::clone_specs(&config.sync)?;
        Ok(sync::public_warnings(&lessons_home, &clones))
    });
    match warnings {
        Ok(warning_lines) => warning_lines.iter().for_each(|warning_line| {
            // A closed stderr (a hook whose reader went away) must not turn a caution into a panic.
            drop(writeln!(std::io::stderr(), "{warning_line}"));
        }),
        Err(skipped) => journal!("public warning skipped: {skipped:#}"),
    }
}

/// Syncs the configured clones (all of them, or the ones carrying `--domain`) and exits as `sync::exit_for` says.
fn run_sync(lessons_home: &Path, requested: Option<Domain>) -> Result<ExitCode> {
    let config: config::Config = config::load(lessons_home)?;
    sync::sync_configured(lessons_home, &config.sync, requested, &[]).map(sync::exit_for)
}

/// Runs the prompt hook. It never fails: every error goes to stderr and the exit code stays 0,
/// because exit code 2 would block the user's prompt.
fn run_hook(hook_event: HookEvent, fn_lessons_home: impl Fn() -> Result<PathBuf>) {
    let hook_result: Result<String> = match hook_event {
        HookEvent::UserPromptSubmit => fn_lessons_home().and_then(|lessons_home| {
            hook::user_prompt_submit(&lessons_home, std::io::stdin().lock())
        }),
    };
    match hook_result {
        Ok(hook_text) => print!("{hook_text}"),
        Err(hook_error) => eprintln!("lessons hook: {hook_error:#}"),
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
        // a leftover file in the temp dir is harmless, so a failed delete is ignored
        std::fs::remove_file(template_path).ok();
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

#[cfg(unix)]
const OWNER_ONLY_FILE_MODE: u32 = 0o600; // read and write for the owner, nothing for group or others

/// Creates a new file readable by its owner only: the template may hold work lesson text, and the
/// temp dir is shared by every user of the machine. `create_new` also refuses a pre-planted file.
fn write_private_file(file_path: &Path, file_text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut mut_open_options = std::fs::OpenOptions::new();
    mut_open_options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut mut_open_options, OWNER_ONLY_FILE_MODE);
    mut_open_options
        .open(file_path)?
        .write_all(file_text.as_bytes())
}

/// Opens the template in the user's editor and parses the result. Returns the template's path,
/// which the caller deletes only once the lesson is stored.
fn new_lesson_from_editor() -> Result<(NewLesson, PathBuf)> {
    let template_path: PathBuf =
        std::env::temp_dir().join(format!("lessons-{}.toml", ulid::Ulid::generate()));
    write_private_file(&template_path, EDIT_TEMPLATE)
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
    let found: FoundLesson = store::find_in_domains(&opened, lesson_id)?
        .with_context(|| lesson_not_found(lessons_home, &opened, lesson_id))?;
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

/// The parsed `--in` value. A newtype because clap reads a bare `Vec` field as a repeatable flag.
#[derive(Clone, Debug)]
struct DomainList(Vec<Domain>);

/// Parses `--in`: `all`, or a comma-separated list of domain names, without repeats.
fn parse_domain_list(domains_text: &str) -> std::result::Result<DomainList, String> {
    let fn_parse_one = |domain_name: &str| {
        Domain::from_str(domain_name.trim(), true).map_err(|_| {
            format!("unknown domain `{domain_name}`; use work, personal, shared or all")
        })
    };
    if domains_text.trim().eq_ignore_ascii_case("all") {
        Ok(DomainList(Domain::ALL.to_vec()))
    } else {
        let parsed: Vec<Domain> = domains_text
            .split(',')
            .map(fn_parse_one)
            .collect::<std::result::Result<Vec<Domain>, String>>()?;
        Ok(DomainList(
            Domain::ALL
                .into_iter()
                .filter(|domain| parsed.contains(domain))
                .collect(),
        )) // fixed order, and a repeated name opens its file once
    }
}

/// The error text for an id found in none of the open files, naming what was searched.
fn lesson_not_found(
    lessons_home: &Path,
    opened: &[(Domain, rusqlite::Connection)],
    lesson_id: &str,
) -> String {
    let searched_names: Vec<&str> = opened
        .iter()
        .map(|(domain, _conn)| domain.as_str())
        .collect();
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
}

fn run_list(
    lessons_home: &Path,
    in_domains: &[Domain],
    limit: usize,
    tag: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let opened = db::open_existing_domains(lessons_home, in_domains)?;
    // Each file returns its newest `limit`; merged and cut again, that is the newest `limit` overall.
    let per_file: Vec<Vec<FoundLesson>> = opened
        .iter()
        .map(|(domain, conn)| {
            store::list(conn, limit, tag).map(|lessons| {
                lessons
                    .into_iter()
                    .map(|lesson| FoundLesson {
                        domain: domain.as_str(),
                        lesson,
                    })
                    .collect()
            })
        })
        .collect::<Result<Vec<Vec<FoundLesson>>>>()?;
    let mut mut_newest: Vec<FoundLesson> = per_file.into_iter().flatten().collect();
    mut_newest.sort_by(|left_lesson, right_lesson| {
        right_lesson
            .lesson
            .created_at
            .cmp(&left_lesson.lesson.created_at)
            .then_with(|| right_lesson.lesson.id.cmp(&left_lesson.lesson.id))
    });
    mut_newest.truncate(limit);
    match format {
        OutputFormat::Text => mut_newest.iter().for_each(|found| {
            let retired_note: &str = if found.lesson.superseded_by.is_some() {
                " (retired)"
            } else {
                ""
            };
            println!(
                "{} [{}] {}{retired_note}",
                found.lesson.id, found.domain, found.lesson.title
            );
        }),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&mut_newest)?),
    }
    Ok(())
}

fn run_update(lessons_home: &Path, lesson_id: &str, patch: LessonPatch) -> Result<()> {
    let opened = db::open_existing_domains(lessons_home, &Domain::ALL)?;
    let (domain, conn) = store::owner_of(&opened, lesson_id)?
        .with_context(|| lesson_not_found(lessons_home, &opened, lesson_id))?;
    if !store::update(conn, lesson_id, &patch)? {
        bail!(lesson_not_found(lessons_home, &opened, lesson_id)); // gone between the lookup and the update
    }
    println!("updated {lesson_id} [{}]", domain.as_str());
    Ok(())
}

fn run_hit(lessons_home: &Path, lesson_id: &str) -> Result<()> {
    let opened = db::open_existing_domains(lessons_home, &Domain::ALL)?;
    let (domain, conn) = store::owner_of(&opened, lesson_id)?
        .with_context(|| lesson_not_found(lessons_home, &opened, lesson_id))?;
    let new_hits: i64 = store::bump_hits(conn, lesson_id)?
        .with_context(|| lesson_not_found(lessons_home, &opened, lesson_id))?; // gone between the lookup and the update
    println!("hit {lesson_id} [{}]: {new_hits} hits", domain.as_str());
    Ok(())
}

fn run_retire(lessons_home: &Path, lesson_id: &str, superseded_by_id: &str) -> Result<()> {
    if lesson_id == superseded_by_id {
        bail!("a lesson cannot replace itself");
    }
    let opened = db::open_existing_domains(lessons_home, &Domain::ALL)?;
    // The replacement may live in another file, so no foreign key can enforce it; check here so a
    // typo cannot hide a lesson behind an id that does not exist.
    store::owner_of(&opened, superseded_by_id)?.with_context(|| {
        format!(
            "the replacement: {}",
            lesson_not_found(lessons_home, &opened, superseded_by_id)
        )
    })?;
    let (domain, conn) = store::owner_of(&opened, lesson_id)?
        .with_context(|| lesson_not_found(lessons_home, &opened, lesson_id))?;
    if !store::retire(conn, lesson_id, superseded_by_id)? {
        bail!(lesson_not_found(lessons_home, &opened, lesson_id)); // gone between the lookup and the update
    }
    println!(
        "retired {lesson_id} [{}], superseded by {superseded_by_id}",
        domain.as_str()
    );
    Ok(())
}

fn run_search(
    lessons_home: &Path,
    search_text: &str,
    in_domains: Option<DomainList>,
    limit: usize,
    raw: bool,
    format: OutputFormat,
) -> Result<()> {
    let domains: Vec<Domain> = match in_domains {
        Some(domain_list) => domain_list.0,
        None => config::load(lessons_home)?.default_search,
    };
    // Raw text goes through the cleaner unless the user asked to write FTS5 syntax themselves.
    let fts_query: Option<String> = if raw {
        Some(search_text.to_string())
    } else {
        query::fts_query_from_text(search_text)
    };
    journal!("search: {search_text:?} -> {fts_query:?} in {domains:?}");
    let hits: Vec<SearchHit> = match &fts_query {
        Some(fts_query) => {
            let opened = db::open_existing_domains(lessons_home, &domains)?;
            store::search_all(&opened, fts_query, limit).with_context(|| match raw {
                true => format!("invalid FTS5 query from --raw: {fts_query}"),
                false => format!("search failed for {fts_query}"),
            })?
        }
        None => Vec::new(), // only stopwords or punctuation: nothing to look for, which is not an error
    };
    match format {
        OutputFormat::Text => print!(
            "{}",
            hits.iter().map(search_hit_as_text).collect::<String>()
        ),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&hits)?),
    }
    Ok(())
}

/// Three compact lines per hit: id and title, then the first line of the problem and of the fix.
fn search_hit_as_text(hit: &SearchHit) -> String {
    let fn_first_line = |text: &str| text.lines().next().unwrap_or("").to_string();
    let fix_line: String = hit
        .lesson
        .resolution
        .as_deref()
        .map_or(String::new(), |resolution| {
            format!("  {:<LABEL_WIDTH$}{}\n", "fix", fn_first_line(resolution))
        });
    format!(
        "{} [{}] {}\n  {:<LABEL_WIDTH$}{}\n{fix_line}",
        hit.lesson.id,
        hit.domain,
        hit.lesson.title,
        "problem",
        fn_first_line(&hit.lesson.problem)
    )
}

fn run_move(lessons_home: &Path, lesson_id: &str, target_domain: Domain) -> Result<()> {
    let target_conn = db::open_target(lessons_home, target_domain)?;
    // The source is looked for in the OTHER files only: after a crash mid-move the lesson is in
    // both, and finding the target copy first would wrongly report the move as already done.
    let other_domains: Vec<Domain> = Domain::ALL
        .into_iter()
        .filter(|domain| *domain != target_domain)
        .collect();
    let other_files = db::open_existing_domains(lessons_home, &other_domains)?;
    let source_domain: Domain = match store::owner_of(&other_files, lesson_id)? {
        Some((domain, _conn)) => *domain,
        None if store::find_by_id(&target_conn, lesson_id)?.is_some() => {
            bail!("{lesson_id} is already in {}", target_domain.as_str())
        }
        None => bail!("no lesson with id {lesson_id} in any DB file"),
    };
    drop(other_files); // only one connection to the source from here on, and that one writes
    let mut mut_source_conn = db::open(&source_domain.file_path(lessons_home), OpenMode::Existing)?;
    if !store::move_lesson(&mut mut_source_conn, &target_conn, lesson_id)? {
        bail!(
            "{lesson_id} disappeared from {} before it could be moved",
            source_domain.as_str()
        );
    }
    println!(
        "moved {lesson_id} [{}] -> [{}]",
        source_domain.as_str(),
        target_domain.as_str()
    );
    Ok(())
}
