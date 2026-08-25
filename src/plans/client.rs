use std::env;

use anyhow::{Context, Result, bail};
use reqwest::blocking::{RequestBuilder, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Clone)]
pub struct Client {
    endpoint: String,
    http: reqwest::blocking::Client,
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProjectSummary {
    pub slug: String,
}

#[derive(Debug, Deserialize)]
pub struct Plan {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub project: Option<ProjectSummary>,
    #[serde(default)]
    pub entry_path: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub files: Vec<PlanFile>,
}

#[derive(Debug, Deserialize)]
pub struct PlanFile {
    pub path: String,
    #[serde(default)]
    pub content_type: String,
    #[serde(default)]
    pub byte_size: u64,
}

#[derive(Debug, Deserialize)]
pub struct Project {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub plan_count: u64,
}

#[derive(Debug, Deserialize)]
pub struct MediaItem {
    pub id: String,
    pub key: String,
    pub url: String,
    #[serde(default)]
    pub byte_size: u64,
}

#[derive(Deserialize)]
pub struct PlanResponse {
    pub plan: Plan,
    #[serde(default)]
    pub location: Option<String>,
}

#[derive(Deserialize)]
struct PlansResponse {
    plans: Vec<Plan>,
}

#[derive(Deserialize)]
struct ProjectsResponse {
    projects: Vec<Project>,
}

#[derive(Deserialize)]
struct ProjectResponse {
    project: Project,
}

#[derive(Deserialize)]
struct MediaListResponse {
    media: Vec<MediaItem>,
}

#[derive(Serialize)]
struct MoveRequest<'a> {
    project: Option<&'a str>,
}

impl Client {
    /// Caddy on the tailnet injects the service credential. The bearer token in
    /// `AGENTS_PLANS_TOKEN` is only needed to reach the Worker directly.
    pub fn new(endpoint: String) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .build()
            .context("could not create the HTTP client")?;
        Ok(Self {
            endpoint,
            http,
            token: env::var("AGENTS_PLANS_TOKEN")
                .ok()
                .filter(|token| !token.is_empty()),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn plan_url(&self, id: &str) -> String {
        format!("{}/plans/{id}", self.endpoint)
    }

    pub fn get(&self, path: &str) -> RequestBuilder {
        self.authorize(self.http.get(self.url(path)))
    }

    pub fn post(&self, path: &str) -> RequestBuilder {
        self.authorize(self.http.post(self.url(path)))
    }

    pub fn patch(&self, path: &str) -> RequestBuilder {
        self.authorize(self.http.patch(self.url(path)))
    }

    pub fn put_absolute(&self, url: &str) -> RequestBuilder {
        self.http.put(url)
    }

    pub fn list_plans(&self, path: &str, query: &[(String, String)]) -> Result<Vec<Plan>> {
        let response = self.get(path).query(query).send()?;
        Ok(self.json::<PlansResponse>(response)?.plans)
    }

    pub fn show_plan(&self, id: &str) -> Result<Plan> {
        let response = self.get(&format!("/api/plans/{id}")).send()?;
        Ok(self.json::<PlanResponse>(response)?.plan)
    }

    pub fn move_plan(&self, id: &str, project: Option<&str>) -> Result<Plan> {
        let response = self
            .patch(&format!("/api/plans/{id}"))
            .json(&MoveRequest { project })
            .send()?;
        Ok(self.json::<PlanResponse>(response)?.plan)
    }

    pub fn delete_plan(&self, id: &str) -> Result<()> {
        let response = self
            .authorize(self.http.delete(self.url(&format!("/api/plans/{id}"))))
            .send()?;
        self.empty(response)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let response = self.get("/api/projects").send()?;
        Ok(self.json::<ProjectsResponse>(response)?.projects)
    }

    pub fn add_project(&self, slug: &str, name: &str) -> Result<Project> {
        let response = self
            .post("/api/projects")
            .json(&serde_json::json!({ "slug": slug, "name": name }))
            .send()?;
        Ok(self.json::<ProjectResponse>(response)?.project)
    }

    pub fn list_media(&self) -> Result<Vec<MediaItem>> {
        let response = self.get("/api/media").send()?;
        Ok(self.json::<MediaListResponse>(response)?.media)
    }

    pub fn delete_media(&self, id: &str) -> Result<()> {
        let response = self
            .authorize(self.http.delete(self.url(&format!("/api/media/{id}"))))
            .send()?;
        self.empty(response)
    }

    pub fn json<T: DeserializeOwned>(&self, response: Response) -> Result<T> {
        let response = checked(response)?;
        response.json().context("the server returned invalid JSON")
    }

    pub fn empty(&self, response: Response) -> Result<()> {
        checked(response)?;
        Ok(())
    }

    /// Turns a relative `Location` header into an absolute URL.
    pub fn resolve_location(&self, location: &str) -> String {
        if location.starts_with("http://") || location.starts_with("https://") {
            location.to_owned()
        } else if location.starts_with('/') {
            format!("{}{}", self.endpoint, location)
        } else {
            format!("{}/{}", self.endpoint, location)
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.endpoint, path)
    }

    fn authorize(&self, builder: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }
}

fn checked(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let body = response.text().unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| body.trim().to_owned());
    if message.is_empty() {
        bail!("request failed with {status}");
    }
    bail!("request failed with {status}: {message}");
}
