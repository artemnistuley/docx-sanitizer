use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use docx_sanitizer::part::{ClassifiedPart, inspect_parts};
use docx_sanitizer::policy::{SanitizeMode, Scope};
use docx_sanitizer::relationships::Relationships;
use docx_sanitizer::report::Report;
use docx_sanitizer::sanitize::{SanitizeResult, report as build_report, sanitize};
use docx_sanitizer::xml::text::ReplacementMode;
use docx_sanitizer::zip::{FileRegistry, MAIN_DOCUMENT_PART, require_part, unpack_docx};

#[derive(Parser)]
#[command(name = "docx-sanitizer", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct PolicyArgs {
    /// Produce output even if unsupported content is present, instead of
    /// refusing to write anything (the default, strict behavior).
    #[arg(long)]
    best_effort: bool,
    /// Only sanitize these comma-separated categories (headers, footers,
    /// comments, footnotes, endnotes, docprops, revisions).
    /// `word/document.xml` is always sanitized regardless. Conflicts with
    /// --exclude.
    #[arg(long, conflicts_with = "exclude")]
    include: Option<String>,
    /// Sanitize every category except these comma-separated ones.
    /// Conflicts with --include.
    #[arg(long)]
    exclude: Option<String>,
    /// Replacement strategy for visible/revision text (`w:t`/`w:delText`):
    /// preserve-length, constant, or clear. Does not affect
    /// author/initials/date or docProps values, which have their own fixed
    /// defaults.
    #[arg(long, default_value = "preserve-length")]
    mode: String,
    /// Collapse tracked changes to their accepted state before sanitizing
    /// (deleted text removed, inserted text kept and unwrapped). Off by
    /// default -- track-changes structure is preserved, per DESIGN.md.
    #[arg(long)]
    remove_track_changes: bool,
    /// Replace `word/media/*` images with a fixed placeholder instead of
    /// leaving them as unsupported content. Only png/jpg/jpeg/gif/bmp are
    /// covered; other formats (e.g. emf/wmf) remain unsupported regardless.
    /// Works independently of --best-effort: with this flag, strict mode
    /// no longer blocks on media with a supported extension.
    #[arg(long)]
    strip_media: bool,
}

impl PolicyArgs {
    fn sanitize_mode(&self) -> SanitizeMode {
        if self.best_effort { SanitizeMode::BestEffort } else { SanitizeMode::Strict }
    }

    fn scope(&self) -> Result<Scope, String> {
        match (self.include.as_deref(), self.exclude.as_deref()) {
            (Some(spec), None) => Scope::parse_include(spec).map_err(|err| err.to_string()),
            (None, Some(spec)) => Scope::parse_exclude(spec).map_err(|err| err.to_string()),
            (None, None) => Ok(Scope::all()),
            (Some(_), Some(_)) => unreachable!("clap enforces --include/--exclude are mutually exclusive"),
        }
    }

    fn replacement_mode(&self) -> Result<ReplacementMode, String> {
        ReplacementMode::parse(&self.mode).map_err(|err| err.to_string())
    }
}

#[derive(Subcommand)]
enum Command {
    /// Inspect a DOCX file's parts, classification, and support tier.
    Inspect { input: PathBuf },
    /// Sanitize a DOCX file's document body text.
    Sanitize {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[command(flatten)]
        policy: PolicyArgs,
        /// Also write a JSON sanitization report to this path.
        #[arg(long)]
        report_json: Option<PathBuf>,
    },
    /// Report what a sanitize run would do, without writing a `.docx`.
    Report {
        input: PathBuf,
        #[command(flatten)]
        policy: PolicyArgs,
        /// Write the JSON report to this path instead of stdout.
        #[arg(long)]
        report_json: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { input } => run_inspect(&input),
        Command::Sanitize { input, output, policy, report_json } => {
            let (mode, scope, replacement_mode) = match resolve_policy(&policy) {
                Ok(resolved) => resolved,
                Err(err) => {
                    eprintln!("error: {err}");
                    return ExitCode::FAILURE;
                }
            };
            run_sanitize(
                &input,
                &output,
                mode,
                &scope,
                replacement_mode,
                policy.remove_track_changes,
                policy.strip_media,
                report_json.as_deref(),
            )
        }
        Command::Report { input, policy, report_json } => {
            let (mode, scope, replacement_mode) = match resolve_policy(&policy) {
                Ok(resolved) => resolved,
                Err(err) => {
                    eprintln!("error: {err}");
                    return ExitCode::FAILURE;
                }
            };
            run_report(
                &input,
                mode,
                &scope,
                replacement_mode,
                policy.remove_track_changes,
                policy.strip_media,
                report_json.as_deref(),
            )
        }
    }
}

fn resolve_policy(policy: &PolicyArgs) -> Result<(SanitizeMode, Scope, ReplacementMode), String> {
    Ok((policy.sanitize_mode(), policy.scope()?, policy.replacement_mode()?))
}

fn run_inspect(input: &PathBuf) -> ExitCode {
    let files = match open_and_unpack(input) {
        Ok(files) => files,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let parts = match inspect_parts(&files) {
        Ok(parts) => parts,
        Err(err) => {
            eprintln!("error: failed to classify parts: {err}");
            return ExitCode::FAILURE;
        }
    };

    print_table(&parts);
    ExitCode::SUCCESS
}

#[allow(clippy::too_many_arguments)]
fn run_sanitize(
    input: &PathBuf,
    output: &PathBuf,
    mode: SanitizeMode,
    scope: &Scope,
    replacement_mode: ReplacementMode,
    remove_track_changes: bool,
    strip_media: bool,
    report_json: Option<&std::path::Path>,
) -> ExitCode {
    let files = match open_and_unpack(input) {
        Ok(files) => files,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(path) = report_json
        && let Err(err) = write_report(
            &files,
            mode,
            scope,
            replacement_mode,
            remove_track_changes,
            strip_media,
            Some(path),
        )
    {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }

    let result = match sanitize(&files, mode, scope, replacement_mode, remove_track_changes, strip_media) {
        Ok(result) => result,
        Err(err) => {
            eprintln!("error: failed to sanitize document: {err}");
            return ExitCode::FAILURE;
        }
    };

    match result {
        SanitizeResult::Blocked { concerns } => {
            eprintln!("error: refusing to write output -- unsupported content found:");
            for concern in &concerns {
                eprintln!("  {}: {}", concern.part, concern.description);
            }
            eprintln!("hint: pass --best-effort to produce output anyway");
            ExitCode::FAILURE
        }
        SanitizeResult::Produced(output_data) => {
            for finding in &output_data.unsupported {
                eprintln!("warning: {}: {}", finding.part, finding.description);
            }

            if let Err(err) = std::fs::write(output, output_data.bytes) {
                eprintln!("error: failed to write {}: {err}", output.display());
                return ExitCode::FAILURE;
            }

            ExitCode::SUCCESS
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_report(
    input: &PathBuf,
    mode: SanitizeMode,
    scope: &Scope,
    replacement_mode: ReplacementMode,
    remove_track_changes: bool,
    strip_media: bool,
    report_json: Option<&std::path::Path>,
) -> ExitCode {
    let files = match open_and_unpack(input) {
        Ok(files) => files,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    match write_report(
        &files,
        mode,
        scope,
        replacement_mode,
        remove_track_changes,
        strip_media,
        report_json,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_report(
    files: &FileRegistry,
    mode: SanitizeMode,
    scope: &Scope,
    replacement_mode: ReplacementMode,
    remove_track_changes: bool,
    strip_media: bool,
    report_json: Option<&std::path::Path>,
) -> Result<(), String> {
    let report: Report = build_report(files, mode, scope, replacement_mode, remove_track_changes, strip_media)
        .map_err(|err| format!("failed to build report: {err}"))?;
    let json = serde_json::to_string_pretty(&report)
        .map_err(|err| format!("failed to serialize report: {err}"))?;

    match report_json {
        Some(path) => std::fs::write(path, json)
            .map_err(|err| format!("failed to write {}: {err}", path.display())),
        None => {
            println!("{json}");
            Ok(())
        }
    }
}

fn open_and_unpack(input: &PathBuf) -> Result<FileRegistry, String> {
    let file = std::fs::File::open(input)
        .map_err(|err| format!("failed to open {}: {err}", input.display()))?;
    let files =
        unpack_docx(file).map_err(|err| format!("failed to read docx package: {err}"))?;
    let relationships = Relationships::from_files(&files)
        .map_err(|err| format!("failed to read relationships: {err}"))?;
    let main_document_path = relationships.main_document_path().unwrap_or(MAIN_DOCUMENT_PART);
    require_part(&files, main_document_path).map_err(|err| err.to_string())?;
    Ok(files)
}

fn print_table(parts: &[ClassifiedPart]) {
    let path_header = "PATH";
    let kind_header = "KIND";
    let tier_header = "TIER";

    let path_width = parts
        .iter()
        .map(|part| part.path.len())
        .max()
        .unwrap_or(0)
        .max(path_header.len());
    let kind_width = parts
        .iter()
        .map(|part| part.kind.to_string().len())
        .max()
        .unwrap_or(0)
        .max(kind_header.len());

    println!("{path_header:path_width$}  {kind_header:kind_width$}  {tier_header}");
    for part in parts {
        let kind = part.kind.to_string();
        println!("{:path_width$}  {kind:kind_width$}  {}", part.path, part.tier);
    }
}
