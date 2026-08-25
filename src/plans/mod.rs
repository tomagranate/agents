mod client;
mod infer_project;
mod media;
mod upload;

use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, Subcommand, ValueEnum};
use serde::Deserialize;

use crate::{config::Paths, util};
use client::{Client, MediaItem, Plan, Project};

const DEFAULT_ENDPOINT: &str = "https://plans.tomagranate.com";

/// Optional endpoint override in `~/.config/agents/plans.toml`.
#[derive(Deserialize)]
struct PlansConfig {
    endpoint: Option<String>,
}

#[derive(Debug, Args)]
pub struct PlansArgs {
    #[command(subcommand)]
    command: PlansCommand,
    /// Override the configured plans service endpoint.
    #[arg(long, global = true)]
    endpoint: Option<String>,
}

#[derive(Debug, Args)]
pub struct MediaArgs {
    #[command(subcommand)]
    command: MediaCommand,
    /// Override the configured plans service endpoint.
    #[arg(long, global = true)]
    endpoint: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum PlansCommand {
    /// Upload an HTML plan file or directory.
    Upload(UploadArgs),
    /// List plans.
    Ls(ListArgs),
    /// Show one plan.
    Show { id: String },
    /// Move a plan to a project or to no project.
    Mv(MoveArgs),
    /// Remove a plan.
    Rm { id: String },
    /// Search plan content.
    Search(SearchArgs),
    /// Open the plans site or one plan in a browser.
    Open { id: Option<String> },
    /// List or add projects.
    Projects {
        #[command(subcommand)]
        command: Option<ProjectsCommand>,
    },
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("membership").multiple(false).args(["project", "no_project"])))]
pub struct UploadArgs {
    file_or_dir: PathBuf,
    /// Project slug. The command infers one from the current directory.
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    title: Option<String>,
    /// Store the plan without a project.
    #[arg(long)]
    no_project: bool,
    /// Entry file for directory uploads.
    #[arg(long, default_value = "index.html")]
    entry: String,
    /// Replace the content of one plan.
    #[arg(long)]
    replace: Option<String>,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("membership").multiple(false).args(["project", "no_project"])))]
pub struct ListArgs {
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    no_project: bool,
    #[arg(long, value_enum, conflicts_with = "older_than")]
    since: Option<Age>,
    #[arg(long, value_enum, conflicts_with = "since")]
    older_than: Option<OlderThan>,
    #[arg(long)]
    query: Option<String>,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("destination").required(true).multiple(false).args(["project", "no_project"])))]
pub struct MoveArgs {
    id: String,
    project: Option<String>,
    #[arg(long)]
    no_project: bool,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("membership").multiple(false).args(["project", "no_project"])))]
pub struct SearchArgs {
    query: String,
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    no_project: bool,
    #[arg(long, value_enum)]
    since: Option<Age>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Age {
    #[value(name = "7d")]
    Seven,
    #[value(name = "30d")]
    Thirty,
    #[value(name = "90d")]
    Ninety,
}

impl Age {
    fn as_str(self) -> &'static str {
        match self {
            Self::Seven => "7d",
            Self::Thirty => "30d",
            Self::Ninety => "90d",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OlderThan {
    #[value(name = "90d")]
    NinetyDays,
}

impl OlderThan {
    fn as_str(self) -> &'static str {
        match self {
            Self::NinetyDays => "90d",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ProjectsCommand {
    /// Add a project.
    Add {
        slug: String,
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MediaCommand {
    /// Upload public media.
    Put {
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// List public media.
    Ls,
    /// Remove public media by id or key.
    Rm { id_or_key: String },
}

pub fn run(paths: &Paths, args: PlansArgs) -> Result<()> {
    let client = Client::new(endpoint(paths, args.endpoint)?)?;

    match args.command {
        PlansCommand::Upload(args) => {
            // --replace overwrites content only. Membership changes on
            // replace need an explicit --project or --no-project; inferring
            // one from the current directory would silently move the plan.
            let project = if args.replace.is_some() && args.project.is_none() && !args.no_project {
                None
            } else {
                Some(infer_project::selected_project(
                    args.project,
                    args.no_project,
                )?)
            };
            let url = upload::upload(
                &client,
                upload::UploadOptions {
                    path: args.file_or_dir,
                    project,
                    title: args.title,
                    entry: args.entry,
                    replace: args.replace,
                },
            )?;
            println!("{url}");
        }
        PlansCommand::Ls(args) => {
            let query = list_query(&args);
            print_plans(&client.list_plans("/api/plans", &query)?);
        }
        PlansCommand::Show { id } => print_plan(&client.show_plan(&id)?),
        PlansCommand::Mv(args) => {
            let plan = client.move_plan(&args.id, args.project.as_deref())?;
            print_plan(&plan);
        }
        PlansCommand::Rm { id } => {
            client.delete_plan(&id)?;
            println!("Removed plan {id}");
        }
        PlansCommand::Search(args) => {
            let query = search_query(&args);
            print_plans(&client.list_plans("/api/search", &query)?);
        }
        PlansCommand::Open { id } => {
            let url = id
                .map(|id| client.plan_url(&id))
                .unwrap_or_else(|| client.endpoint().to_owned());
            open_browser(&url)?;
        }
        PlansCommand::Projects { command: None } => print_projects(&client.list_projects()?),
        PlansCommand::Projects {
            command: Some(ProjectsCommand::Add { slug, name }),
        } => {
            let project = client.add_project(&slug, name.as_deref().unwrap_or(&slug))?;
            println!("{}\t{}", project.slug, project.name);
        }
    }

    Ok(())
}

/// Media shares the plans service and its endpoint configuration.
pub fn run_media(paths: &Paths, args: MediaArgs) -> Result<()> {
    let client = Client::new(endpoint(paths, args.endpoint)?)?;

    match args.command {
        MediaCommand::Put { file, name } => {
            let item = media::put(&client, file, name)?;
            println!("{}", item.url);
            eprintln!("id: {}\nsize: {} bytes", item.id, item.byte_size);
        }
        MediaCommand::Ls => print_media(&client.list_media()?),
        MediaCommand::Rm { id_or_key } => {
            media::remove(&client, &id_or_key)?;
            println!("Removed media {id_or_key}");
        }
    }

    Ok(())
}

fn endpoint(paths: &Paths, override_endpoint: Option<String>) -> Result<String> {
    if let Some(endpoint) = override_endpoint {
        return validate_endpoint(&endpoint);
    }

    let path = paths.config_dir.join("plans.toml");
    let configured = match fs::read_to_string(&path) {
        Ok(contents) => {
            toml::from_str::<PlansConfig>(&contents)
                .with_context(|| format!("{} is invalid", path.display()))?
                .endpoint
        }
        Err(_) => None,
    };

    validate_endpoint(&configured.unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned()))
}

fn validate_endpoint(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        bail!("endpoint cannot be empty");
    }

    Ok(endpoint.to_owned())
}

fn list_query(args: &ListArgs) -> Vec<(String, String)> {
    let mut query = membership_query(args.project.as_deref(), args.no_project);
    if let Some(since) = args.since {
        query.push(("since".to_owned(), since.as_str().to_owned()));
    }
    if let Some(older_than) = args.older_than {
        query.push(("older_than".to_owned(), older_than.as_str().to_owned()));
    }
    if let Some(value) = &args.query {
        query.push(("q".to_owned(), value.to_owned()));
    }
    query
}

fn search_query(args: &SearchArgs) -> Vec<(String, String)> {
    let mut query = membership_query(args.project.as_deref(), args.no_project);
    query.push(("q".to_owned(), args.query.to_owned()));
    if let Some(since) = args.since {
        query.push(("since".to_owned(), since.as_str().to_owned()));
    }
    query
}

fn membership_query(project: Option<&str>, no_project: bool) -> Vec<(String, String)> {
    if let Some(project) = project {
        vec![("project".to_owned(), project.to_owned())]
    } else if no_project {
        vec![("no_project".to_owned(), "1".to_owned())]
    } else {
        Vec::new()
    }
}

fn print_plans(plans: &[Plan]) {
    println!("ID\tPROJECT\tTITLE\tUPDATED");
    for plan in plans {
        let project = plan
            .project
            .as_ref()
            .map(|project| project.slug.as_str())
            .unwrap_or("No project");
        println!(
            "{}\t{}\t{}\t{}",
            plan.id, project, plan.title, plan.updated_at
        );
    }
}

fn print_plan(plan: &Plan) {
    let project = plan
        .project
        .as_ref()
        .map(|project| project.slug.as_str())
        .unwrap_or("No project");
    println!("ID: {}", plan.id);
    println!("Title: {}", plan.title);
    println!("Project: {project}");
    println!("Entry: {}", plan.entry_path);
    println!("Created: {}", plan.created_at);
    println!("Updated: {}", plan.updated_at);
    if !plan.files.is_empty() {
        println!("Files:");
        for file in &plan.files {
            println!(
                "  {}\t{}\t{} bytes",
                file.path, file.content_type, file.byte_size
            );
        }
    }
}

fn print_projects(projects: &[Project]) {
    println!("SLUG\tNAME\tPLANS");
    for project in projects {
        println!("{}\t{}\t{}", project.slug, project.name, project.plan_count);
    }
}

fn print_media(items: &[MediaItem]) {
    println!("ID\tKEY\tSIZE\tURL");
    for item in items {
        println!(
            "{}\t{}\t{}\t{}",
            item.id, item.key, item.byte_size, item.url
        );
    }
}

fn open_browser(url: &str) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    util::command_status(program, [url], None)
}
