extern crate chrono;
#[macro_use]
extern crate cpython;
extern crate envy;
extern crate failure;
extern crate futures;
extern crate goji;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate lando;
#[macro_use]
extern crate maplit;
extern crate reqwest;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;
extern crate tokio;

// Std lib

use std::collections::{BTreeMap, HashMap};

// Third party
use chrono::{Datelike, Duration, Local, Weekday};
use failure::Fail;
use goji::{Credentials, Issue, Jira};
use lando::RequestExt;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use reqwest::Client;

lazy_static! {
    static ref STATUS_EMOJI: HashMap<String, &'static str> = {
        hashmap! {
        "In Progress".into() => "👩🏻‍💻",
        "In Review".into() => "👩🏼‍🔬",
        "Closed".into() => "🎉"
        }
    };
}

/// app configuration ( sourced from env variables )
#[derive(Deserialize)]
struct Config {
    pd_token: String,
    pd_team_ids: Vec<String>,
    jira_host: String,
    jira_user: String,
    jira_password: String,
}

/// Slack request payload for commands
/// only the fields we're using are represented
/// more are availbale
#[derive(Deserialize, Debug)]
struct CommandRequest {
    response_url: String,
}

#[derive(Deserialize, Debug)]
struct Incidents {
    incidents: Vec<Incident>,
}

#[derive(Deserialize, Debug)]
struct Incident {
    incident_number: usize,
    title: String,
    status: String,
    html_url: String,
}

gateway!(|request, _| {
    let config = envy::from_env::<Config>()?;
    let slack_url = request
        .payload::<CommandRequest>()
        .map_err(|s| s.compat())?
        .expect("expected payload")
        .response_url;
    if let Err(_) = debrief(config, slack_url) {
        println!("err debriefing");
    }
    Ok(lando::Response::new(()))
});

fn owner(issue: Issue, status: &str) -> Option<String> {
    match status {
        "Closed" => None, // everyone owns this
        _ => Some(format!("@{}", issue.assignee().map(|user| user.name).unwrap_or_else(|| String::from("nobody")))
    }
}

fn issue_display(issue: Issue, jira: &Jira, status: &str) -> String {
    format!(
        "<{}|{}> {}{}",
        issue.permalink(&jira),
        issue.key,
        issue.summary().unwrap_or_else(|| "no summary".into()),
        owner(issue, status).unwrap_or_else(|| String::new())
    )
}

fn debrief(config: Config, slack_url: String) -> Result<(), String> {
    println!("fetching debrief info...");
    let jira = match Jira::new(
        config.jira_host,
        Credentials::Basic(config.jira_user, config.jira_password),
    ) {
        Ok(j) => j,
        Err(err) => {
            return Err(format!("jira client err: {}", err));
        }
    };

    // how was the weather?
    let teams = config
        .pd_team_ids
        .iter()
        .map(|id| format!("team_ids%5B%5D={}", id))
        .collect::<Vec<_>>()
        .join("&");
    let lookback_days = if Local::now().weekday() == Weekday::Mon {
        3
    } else {
        1
    };
    let since = (Local::now() - Duration::days(lookback_days)).format("%F");
    let pd_query = format!(
        "https://api.pagerduty.com/incidents?statuses%5B%5D=triggered&statuses%5B%5D=acknowledged&{}&since={}",
        teams, since
    );
    let incidents = Client::new()
        .get(&pd_query)
        .header(ACCEPT, "application/vnd.pagerduty+json;version=2")
        .header(AUTHORIZATION, format!("Token token={}", config.pd_token))
        .send()
        .and_then(|mut response| {
            response
                .json::<Incidents>()
                .map(|incidents| incidents.incidents)
        })
        .unwrap_or_default();
    let incidents_response = incidents.into_iter().fold(
        String::from("⛅ *Weather Report*\n"),
        |mut result, incident| {
            result.push_str(
                format!(
                    "<{}|#{}> {} ({})\n",
                    incident.html_url, incident.incident_number, incident.title, incident.status
                ).as_str(),
            );
            result
        },
    );

    // what shipped?
    let mut issues = jira
        .search()
        .iter(
            format!(
                r#"project = "Core Services" AND status in (Closed) and resolutiondate >= -{}d"#,
                lookback_days
            ),
            &Default::default(),
        )
        .map(|iter| iter.collect::<Vec<_>>())
        .unwrap_or_default();

    // what's in flight
    let in_flight = jira
            .search()
            .iter(
                r#"project = "Core Services" AND status in ("In Progress", "In Review") order by status, assignee"#,
                &Default::default(),
            )
            .map(|iter| iter.collect::<Vec<_>>()).unwrap_or_default();

    issues.extend(in_flight);

    // group by ordered status
    let grouped = issues.into_iter().fold(BTreeMap::new(), |mut acc, issue| {
        let status = issue
            .status()
            .map(|status| status.name)
            .unwrap_or_else(|| "Unknown Status".into());
        acc.entry(format!(
            "{} *{}*",
            STATUS_EMOJI.get(&status).unwrap_or_else(|| &&":shrug:"),
            status
        )).or_insert(Vec::new())
            .push(issue_display(issue, &jira, &status));
        acc
    });

    // build response
    let jira_response = grouped
        .into_iter()
        .fold(String::new(), |mut result, (status, issues)| {
            result.push_str(status.as_str());
            result.push('\n');
            result.push_str(issues.join("\n").as_str());
            result.push('\n');
            result
        });

    // send it
    if let Err(err) = Client::new()
        .post(&slack_url)
        .json(&json!({ "text": vec![incidents_response, jira_response].join("\n") }))
        .send()
    {
        println!("failed to debrief on what shipped: {}", err);
    }

    Ok(println!("debriefed"))
}
