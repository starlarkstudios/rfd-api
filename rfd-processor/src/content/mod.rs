// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{borrow::Cow, env, io, path::PathBuf, str::Utf8Error, string::FromUtf8Error};

use async_trait::async_trait;
use base64::DecodeError;
use octorust::Client;
use rfd_data::{
    content::{RfdAsciidoc, RfdAttributes, RfdMarkdown},
    RfdNumber,
};
use rfd_model::schema_ext::ContentFormat;
use tap::TapFallible;
use thiserror::Error;
use tokio::task::JoinError;
use tracing::instrument;
use uuid::Uuid;

use crate::{
    github::{GitHubError, GitHubRfdLocation},
    pdf::RfdPdf,
    util::{decode_base64, write_file, FileIoError},
};

mod asciidoc;

#[derive(Debug, Error)]
pub enum RfdContentError {
    #[error("Failed to decode content file {0}")]
    Decode(#[from] DecodeError),
    #[error("Failed communication with GitHub API {0}")]
    GitHub(#[from] GitHubError),
    #[error("Failed to convert content string {0}")]
    InvalidContent(#[from] Utf8Error),
    #[error("General io failure {0}")]
    Io(#[from] io::Error),
    #[error("File io failure {0}")]
    File(#[from] FileIoError),
    #[error("Failed to parse content")]
    ParserFailed(Result<String, FromUtf8Error>),
    #[error("Failed to run output generator to completion {0}")]
    TaskFailure(#[from] JoinError),
}

#[derive(Debug, Clone)]
pub struct RenderableRfd<'a> {
    content: RfdContent<'a>,
    render_id: Uuid,
}

#[derive(Debug, Clone)]
enum RfdContent<'a> {
    Asciidoc(RfdAsciidoc<'a>),
    Markdown(RfdMarkdown<'a>),
}

impl<'a> RenderableRfd<'a> {
    /// Construct a new RfdContent wrapper that contains Asciidoc content
    pub fn new_asciidoc<T>(content: T) -> Self
    where
        T: Into<Cow<'a, str>>,
    {
        Self {
            content: RfdContent::Asciidoc(RfdAsciidoc::new(content.into())),
            render_id: Uuid::new_v4(),
        }
    }

    /// Construct a new RfdContent wrapper that contains Markdown content
    pub fn new_markdown<T>(content: T) -> Self
    where
        T: Into<Cow<'a, str>>,
    {
        Self {
            content: RfdContent::Markdown(RfdMarkdown::new(content.into())),
            render_id: Uuid::new_v4(),
        }
    }

    /// Get a reference to the internal unparsed contents
    pub fn raw(&self) -> &str {
        match &self.content {
            RfdContent::Asciidoc(adoc) => adoc.raw(),
            RfdContent::Markdown(md) => md.raw(),
        }
    }

    /// Fetch the content that is above the title line
    pub fn header(&self) -> Option<&str> {
        match &self.content {
            RfdContent::Asciidoc(adoc) => adoc.header(),
            RfdContent::Markdown(md) => md.header(),
        }
    }

    /// Fetch the content that is below the title line
    pub fn body(&self) -> Option<&str> {
        match &self.content {
            RfdContent::Asciidoc(adoc) => adoc.body(),
            RfdContent::Markdown(md) => md.body(),
        }
    }

    /// Get an indicator of the inner content format
    pub fn format(&self) -> ContentFormat {
        match self.content {
            RfdContent::Asciidoc(_) => ContentFormat::Asciidoc,
            RfdContent::Markdown(_) => ContentFormat::Markdown,
        }
    }

    /// Consume this wrapper and return the internal unparsed contents
    pub fn into_inner_content(self) -> String {
        match self.content {
            RfdContent::Asciidoc(adoc) => adoc.content.into_owned(),
            RfdContent::Markdown(md) => md.content.into_owned(),
        }
    }

    /// Generate a PDF by combining RFD contents with static resources that are stored for a given
    /// RFD number on a specific branch. Markdown documents do not support PDF generation
    pub async fn to_pdf(
        &self,
        client: &Client,
        number: &RfdNumber,
        branch: &GitHubRfdLocation,
    ) -> Result<RfdPdf, RfdOutputError> {
        match &self.content {
            RfdContent::Asciidoc(adoc) => {
                self.download_images(client, number, branch).await?;

                let pdf = RenderedPdf::render(adoc, self.tmp_path()?).await?;

                self.cleanup_tmp_path()?;

                Ok(RfdPdf {
                    contents: pdf.into_inner(),
                    number: *number,
                })
                // Ok(adoc.to_pdf(client, number, branch).await?)
            }
            _ => Err(RfdOutputError::FormatNotSupported),
        }
    }

    /// Downloads images that are stored on the provided GitHub branch for the given RFD number.
    /// These are stored locally so in a tmp directory for use by asciidoctor
    #[instrument(skip(self, client), fields(storage_path = ?self.tmp_path()))]
    async fn download_images(
        &self,
        client: &Client,
        number: &RfdNumber,
        location: &GitHubRfdLocation,
    ) -> Result<(), RfdContentError> {
        let dir = number.repo_path();
        let storage_path = self.tmp_path()?;

        let images = location.get_images(client, number).await?;

        for image in images {
            let image_path = storage_path.join(
                image
                    .path
                    .replace(dir.trim_start_matches('/'), "")
                    .trim_start_matches('/'),
            );

            let path = PathBuf::from(image_path);
            write_file(&path, &decode_base64(&image.content)?).await?;

            tracing::info!(?path, "Wrote embedded image",);
        }

        Ok(())
    }

    /// Create a tmp directory for rendering this RFD
    fn tmp_path(&self) -> Result<PathBuf, RfdContentError> {
        let mut path = env::temp_dir();
        path.push("rfd-render/");
        path.push(&self.render_id.to_string());

        // Ensure the path exists
        std::fs::create_dir_all(path.clone())?;

        Ok(path)
    }

    // Cleanup remaining images and local state that was used by asciidoctor
    #[instrument(skip(self), fields(storage_path = ?self.tmp_path()), err)]
    fn cleanup_tmp_path(&self) -> Result<(), RfdContentError> {
        let storage_path = self.tmp_path()?;

        if storage_path.exists() && storage_path.is_dir() {
            tracing::info!("Removing temporary content directory {:?}", storage_path);
            std::fs::remove_dir_all(storage_path)
                .tap_err(|err| tracing::warn!(?err, "Failed to clean up temporary files"))?
        }

        Ok(())
    }
}

impl<'a> RfdAttributes for RenderableRfd<'a> {
    fn get_title(&self) -> Option<&str> {
        match &self.content {
            RfdContent::Asciidoc(adoc) => adoc.get_title(),
            RfdContent::Markdown(md) => md.get_title(),
        }
    }

    fn get_state(&self) -> Option<&str> {
        match &self.content {
            RfdContent::Asciidoc(adoc) => adoc.get_state(),
            RfdContent::Markdown(md) => md.get_state(),
        }
    }

    fn update_state(&mut self, value: &str) {
        match &mut self.content {
            RfdContent::Asciidoc(adoc) => adoc.update_state(value),
            RfdContent::Markdown(md) => md.update_state(value),
        }
    }

    fn get_discussion(&self) -> Option<&str> {
        match &self.content {
            RfdContent::Asciidoc(adoc) => adoc.get_discussion(),
            RfdContent::Markdown(md) => md.get_discussion(),
        }
    }

    fn update_discussion(&mut self, value: &str) {
        match &mut self.content {
            RfdContent::Asciidoc(adoc) => adoc.update_discussion(value),
            RfdContent::Markdown(md) => md.update_discussion(value),
        }
    }

    fn get_authors(&self) -> Option<&str> {
        match &self.content {
            RfdContent::Asciidoc(adoc) => adoc.get_authors(),
            RfdContent::Markdown(md) => md.get_authors(),
        }
    }

    fn get_labels(&self) -> Option<&str> {
        match &self.content {
            RfdContent::Asciidoc(adoc) => adoc.get_labels(),
            RfdContent::Markdown(md) => md.get_labels(),
        }
    }

    fn update_labels(&mut self, value: &str) {
        match &mut self.content {
            RfdContent::Asciidoc(adoc) => adoc.update_labels(value),
            RfdContent::Markdown(md) => md.update_labels(value),
        }
    }
}

#[async_trait]
pub trait RfdRenderedFormat<Source> {
    async fn render(content: &Source, content_dir: PathBuf) -> Result<Self, RfdOutputError>
    where
        Self: Sized;
}

pub struct RenderedPdf(Vec<u8>);

impl RenderedPdf {
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

impl From<Vec<u8>> for RenderedPdf {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

#[derive(Debug, Error)]
pub enum RfdOutputError {
    #[error(transparent)]
    Command(#[from] JoinError),
    #[error("Failed to prepare content for output")]
    ContentFailure(#[from] RfdContentError),
    #[error(transparent)]
    File(#[from] FileIoError),
    #[error("Output format is not supported")]
    FormatNotSupported,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
