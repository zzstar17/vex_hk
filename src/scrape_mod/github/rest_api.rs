use std::{fs, path::Path};

use chrono::Utc;
use regex::Regex;

use crate::{
    config::Config, csv_postgres_integration::GeneralizedCsvRecord, osv_schema::OsvEssentials,
};

use super::{api_response::GitHubAdvisoryAPIResponse, GithubApiDownloadError, GithubType};

// https://stackoverflow.com/questions/3809401/what-is-a-good-regular-expression-to-match-a-url
const URL_MATCH: &str = r"https?:\/\/(?:www\.)?[-a-zA-Z0-9@:%._\+~#=]{2,256}\.[a-z]{2,4}\b(?:[-a-zA-Z0-9@:%_\+.~#?&//=]*)";

/// # Retrieve advisories from the api
///
/// <https://docs.github.com/en/rest/security-advisories/global-advisories>
///
/// Saves information relative to one advisory api request, and retrieves all the data from it, even if paginated.
pub struct PaginatedGithubAdvisoriesDataIter<'a> {
    client: &'a reqwest::Client,
    header_next_pattern: Regex,
    request: reqwest::Request,
    finished: bool,
}

impl<'a> PaginatedGithubAdvisoriesDataIter<'a> {
    /// Parse and store the information relative to the request.
    ///
    /// The first request won't happen until [PaginatedGithubAdvisoriesDataIter::next_page_data] is called for the first time.
    ///
    /// See <https://docs.github.com/en/rest/security-advisories/global-advisories> for information on query parameters. This only refers to the query arguments linked to the http query itself, not the headers.
    pub fn new(
        client: &'a reqwest::Client,
        api_url: &'a str,
        token: &'a str,
        query: &[(&str, &str)],
    ) -> Result<Self, reqwest::Error> {
        let next_pattern = Regex::new(&("<(".to_owned() + URL_MATCH + ")>; rel=\"next\"")).unwrap();

        let request = client
            .get(api_url)
            .bearer_auth(token)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header(reqwest::header::USER_AGENT, "User")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .query(&[("per_page", "100")])
            .query(query)
            .build()?;

        Ok(Self {
            client,
            header_next_pattern: next_pattern,
            request,
            finished: false,
        })
    }

    /// Perform a request to get information related to the next page.
    ///
    /// As [PaginatedGithubAdvisoriesDataIter] functions as a iterator, this function will continuously return None if no new information is left to fetch.
    pub async fn next_page_data(
        &mut self,
    ) -> Option<Result<Vec<GitHubAdvisoryAPIResponse>, reqwest::Error>> {
        if self.finished {
            return None;
        }
        Some(self.next_page_data_perform_request().await)
    }

    async fn next_page_data_perform_request(
        &mut self,
    ) -> Result<Vec<GitHubAdvisoryAPIResponse>, reqwest::Error> {
        let response = self
            .client
            .execute(self.request.try_clone().unwrap())
            .await?;

        let next_url_opt = if let Some(link_header) = response.headers().get("link") {
            self.header_next_pattern
                .captures(
                    link_header
                        .to_str()
                        .expect("Failed to convert HTTP header to valid string"),
                )
                .map(|captures| captures.get(1).unwrap().as_str().to_owned())
        } else {
            None
        };

        let data = response
            .json::<crate::scrape_mod::github::api_response::GitHubAdvisoryAPIResponses>()
            .await
            .unwrap();

        if let Some(next_url) = next_url_opt {
            let url = self.request.url_mut();
            *url = reqwest::Url::parse(&next_url)
                .expect("Failed to parse url returned from pagination header");
        } else {
            self.finished = true;
        }

        Ok(data)
    }
}

/// Download and save data in one single csv file, in [crate::csv_postgres_integration::GeneralizedCsvRecord] format
///
/// Download advisories modified after a specific date (inclusive, includes the day itself). Saves everything in a CSV file, where each row corresponds to one advisory. See [crate::csv_postgres_integration] for details.
///
/// Note: this function does NOT save progress during requests, and it won't be able to continue if it gets interrupted or an error occurs, so it should NOT be used for long or error-prone downloads that may require more than the API limit of requests for one hour.
///
/// Returns the number of total entries.
pub async fn api_data_after_update_date_single_csv_file(
    config: &Config,
    client: &reqwest::Client,
    token: &str,
    csv_file_path: &Path,
    date: chrono::NaiveDate,
    ty: GithubType,
) -> Result<usize, GithubApiDownloadError> {
    {
        let parent = csv_file_path.parent().unwrap();
        if !fs::exists(parent)? {
            fs::create_dir_all(parent)?;
        }
    }
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .from_path(csv_file_path)?;

    log::info!(
        "Performing requests to the GitHub API and saving data to CSV. CSV File created at {:?}",
        csv_file_path
    );

    let mut paginated_iter = PaginatedGithubAdvisoriesDataIter::new(
        client,
        &config.github.api.url,
        token,
        &[
            ("published", &date.format(">=%Y-%m-%d").to_string()),
            ("type", ty.api_str()),
        ],
    )?;
    let mut total_entries = 0;
    while let Some(next_page_res) = paginated_iter.next_page_data().await {
        let next_page_data = next_page_res?;
        total_entries += next_page_data.len();

        for advisory in next_page_data {
            let record = GeneralizedCsvRecord::from_github_api_response(advisory);
            writer.write_record(record.as_row())?;
        }
    }
    writer.flush()?;

    Ok(total_entries)
}

/// Download api data and store only names, publish dates and modified dates
///
/// To be used for osv file retrieval
pub async fn get_only_essential_after_modified_date(
    config: &Config,
    client: &reqwest::Client,
    token: &str,
    date: &chrono::DateTime<Utc>,
    ty: GithubType,
) -> Result<Vec<OsvEssentials>, GithubApiDownloadError> {
    let mut paginated_iter = PaginatedGithubAdvisoriesDataIter::new(
        client,
        &config.github.api.url,
        token,
        &[
            ("published", &date.format(">=%Y-%m-%d").to_string()),
            ("type", ty.api_str()),
        ],
    )?;
    let mut data = Vec::new();
    while let Some(next_page_res) = paginated_iter.next_page_data().await {
        let next_page_data = next_page_res?;

        for full_data in next_page_data {
            data.push(OsvEssentials::from_github_api(&full_data));
        }
    }

    Ok(data)
}
