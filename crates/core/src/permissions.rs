//! What SuperCI asks of each cloud and code host, one entry per permission: what it allows, why, and the version
//! that first asked for it. The AWS role's policy is made from this list, and the dashboard compares what an account
//! actually has with it, so a version that needs more says what, and why, before it is allowed.
//!
//! Permissions are only ever added. A feature that uses a new one works without it (it falls back, and the control
//! plane reports it as denied) until the permission is given.
use serde_json::{json, Value};

/// One permission: its id (an AWS statement's Sid, a GitHub permission's name, a GitLab scope), what it allows, why
/// SuperCI needs it, the version that first asked for it, and whether it changes anything (else it only reads).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Need { pub id: &'static str, pub what: &'static str, pub why: &'static str, pub since: &'static str, pub writes: bool }

/// The AWS role a control plane uses for its machines (`superci-plane-<id>`, policy `runner-machines`).
pub const AWS_RUNNER: &[Need] = &[
    Need { id: "LaunchOnlyTaggedMachines", what: "Start machines, each tagged as this control plane's", why: "Each job runs on a machine of its own.", since: "0.1.0", writes: true },
    Need { id: "WhatAMachineIsMadeOf", what: "Use images, disks, network interfaces, security groups and subnets for them", why: "A machine is started from these.", since: "0.1.0", writes: true },
    Need { id: "TagOnLaunch", what: "Tag machines as they start", why: "Only machines tagged as this control plane's can be stopped by it.", since: "0.1.0", writes: true },
    Need { id: "StopOnlyItsOwnMachines", what: "Stop machines tagged as this control plane's", why: "A job's machine ends when the job ends or gets stuck.", since: "0.1.0", writes: true },
    Need { id: "Read", what: "Read images, machines, machine types, spot prices and regions", why: "To choose a machine and see how it is doing.", since: "0.1.0", writes: false },
    Need { id: "Prices", what: "Read AWS's price list", why: "To price on-demand machines and disks at what AWS bills.", since: "0.9.12", writes: false },
    Need { id: "MachineTraffic", what: "Read machines' network counts (CloudWatch)", why: "To add what each machine sent to its cost.", since: "0.9.13", writes: false },
    Need { id: "ReadNetwork", what: "Read its own network (VPC, subnets, security group)", why: "Machines start in a network of their own that lets nothing in.", since: "0.9.21", writes: false },
    Need { id: "SpotServiceRole", what: "Create AWS's spot service role, once", why: "AWS needs it before an account's first spot machine.", since: "0.1.0", writes: true },
];

/// One of `AWS_RUNNER`'s statements, for this account and control plane.
fn aws_statement(id: &str, account_id: &str, plane_id: &str) -> Value {
    let arn = |r: &str| format!("arn:aws:ec2:*:{account_id}:{r}");
    let (action, resource, condition): (Value, Value, Option<Value>) = match id {
        "LaunchOnlyTaggedMachines" => ("ec2:RunInstances".into(), arn("instance/*").into(), Some(json!({ "StringEquals": { "aws:RequestTag/superci-plane": plane_id } }))),
        "WhatAMachineIsMadeOf" => ("ec2:RunInstances".into(), json!(["arn:aws:ec2:*::image/*", arn("volume/*"), arn("network-interface/*"), arn("security-group/*"), arn("subnet/*"), arn("spot-instances-request/*")]), None),
        "TagOnLaunch" => ("ec2:CreateTags".into(), arn("*/*").into(), Some(json!({ "StringEquals": { "ec2:CreateAction": "RunInstances" } }))),
        "StopOnlyItsOwnMachines" => ("ec2:TerminateInstances".into(), arn("instance/*").into(), Some(json!({ "StringEquals": { "aws:ResourceTag/superci-plane": plane_id } }))),
        "Read" => (json!(["ec2:DescribeImages", "ec2:DescribeInstances", "ec2:DescribeInstanceTypeOfferings", "ec2:DescribeSpotPriceHistory", "ec2:DescribeRegions"]), "*".into(), None),
        "Prices" => ("pricing:GetProducts".into(), "*".into(), None),
        "MachineTraffic" => ("cloudwatch:GetMetricStatistics".into(), "*".into(), None),
        "ReadNetwork" => (json!(["ec2:DescribeVpcs", "ec2:DescribeSubnets", "ec2:DescribeSecurityGroups"]), "*".into(), None),
        "SpotServiceRole" => ("iam:CreateServiceLinkedRole".into(), "*".into(), Some(json!({ "StringEquals": { "iam:AWSServiceName": "spot.amazonaws.com" } }))),
        _ => unreachable!("an AWS permission without its statement: {id}"),
    };
    let mut s = json!({ "Sid": id, "Effect": "Allow", "Action": action, "Resource": resource });
    if let Some(c) = condition { s["Condition"] = c }
    s
}

/// The runner role's policy: launch and stop machines tagged with this control plane, and read what that needs.
pub fn aws_runner_policy(account_id: &str, plane_id: &str) -> Value {
    json!({ "Version": "2012-10-17", "Statement": AWS_RUNNER.iter().map(|n| aws_statement(n.id, account_id, plane_id)).collect::<Vec<_>>() })
}

/// What a runner role's policy (as IAM gives it back; None: the role has none) lacks or has otherwise than this version
/// asks: those entries, oldest first.
pub fn aws_runner_missing(have: Option<&Value>, account_id: &str, plane_id: &str) -> Vec<&'static Need> {
    let statements: Vec<&Value> = have.and_then(|p| p["Statement"].as_array()).map(|s| s.iter().collect()).unwrap_or_default();
    AWS_RUNNER.iter().filter(|n| {
        let want = aws_statement(n.id, account_id, plane_id);
        !statements.iter().any(|s| s["Sid"] == n.id && same_statement(s, &want))
    }).collect()
}

/// Not a permission but made with them, by the dashboard with the account's sign-in (the role cannot make networks):
/// the control plane's own network in each region its machines use (`aws::make_network`).
pub const AWS_NETWORK: Need = Need { id: "OwnNetwork", what: "A network of its own in each region its machines use", why: "Nothing can reach a job's machine, and a job cannot reach your other machines in AWS's default network.", since: "0.9.21", writes: true };

/// Two IAM statements alike, with a list of one and the one itself counted the same (IAM may give either back).
fn same_statement(a: &Value, b: &Value) -> bool {
    let norm = |v: &Value| -> Value {
        let mut v = v.clone();
        for k in ["Action", "Resource"] {
            if let Some(list) = v[k].as_array().filter(|l| l.len() == 1) { v[k] = list[0].clone() }
        }
        v
    };
    norm(a) == norm(b)
}

/// The GitHub App's permissions (name, level), by owner kind: an organization's runners register in its runner groups;
/// a personal account's in each repository, which needs administration.
pub fn github(org: bool) -> Vec<(&'static str, &'static str, Need)> {
    let runners = if org {
        ("organization_self_hosted_runners", "write", Need { id: "organization_self_hosted_runners", what: "Self-hosted runners (write)", why: "Each job's runner is registered for it, then removed.", since: "0.1.0", writes: true })
    } else {
        ("administration", "write", Need { id: "administration", what: "Administration (write)", why: "GitHub registers a personal account's runners per repository, which needs it.", since: "0.1.0", writes: true })
    };
    vec![
        runners,
        ("actions", "write", Need { id: "actions", what: "Actions (write)", why: "To see where a run came from (a fork's pull request is not run on your clouds), and to run a job again when AWS took its spot machine back.", since: "0.9.29", writes: true }),
        ("metadata", "read", Need { id: "metadata", what: "Metadata (read)", why: "GitHub gives it to every App.", since: "0.1.0", writes: false }),
    ]
}

/// What GitHub permissions (as an App or an installation has them: name to "read"/"write") lack of what is needed.
pub fn github_missing(have: &Value, org: bool) -> Vec<Need> {
    let rank = |l: &str| match l { "write" | "admin" => 2, "read" => 1, _ => 0 };
    github(org).into_iter().filter(|(name, level, _)| rank(have[*name].as_str().unwrap_or("")) < rank(level)).map(|(_, _, n)| n).collect()
}

/// What the dashboard asks Cloudflare for at a sign-in (OAuth scopes). Nothing is kept in the account: a newer
/// dashboard asks for its own at the next sign-in, and the control plane and runner agent hold no Cloudflare credentials.
/// `offline_access` is what lets the sign-in be renewed (without it, it ends after an hour).
pub const CLOUDFLARE: &[Need] = &[
    Need { id: "account:read", what: "Read your accounts and their usage", why: "To find control planes, and read what containers cost (usage analytics).", since: "0.1.0", writes: false },
    Need { id: "workers_scripts:write", what: "Deploy Workers and set their secrets", why: "The control plane and the runner agent are Workers, updated from here.", since: "0.1.0", writes: true },
    Need { id: "containers:write", what: "Manage container applications and images", why: "Jobs run in containers started from GitHub's runner image.", since: "0.1.0", writes: true },
    Need { id: "offline_access", what: "Stay signed in", why: "So SuperCI on this computer can go on without asking you to sign in every hour. It is kept in SuperCI's own folder; `superci logout` ends it.", since: "0.11.0", writes: false },
];

/// The scopes the dashboard asks Cloudflare for, as OAuth writes them.
pub fn cloudflare_scopes() -> String { CLOUDFLARE.iter().map(|n| n.id).collect::<Vec<_>>().join(" ") }

/// The GitLab token's scopes.
pub const GITLAB: &[Need] = &[
    Need { id: "api", what: "api", why: "To turn projects' job webhooks on and off, and read each job's tags.", since: "0.1.0", writes: true },
    Need { id: "create_runner", what: "create_runner", why: "Each job gets a project runner for its tags.", since: "0.1.0", writes: true },
    Need { id: "manage_runner", what: "manage_runner", why: "Runners are removed when they are no longer needed.", since: "0.1.0", writes: true },
];

/// The scopes a GitLab token (as GitLab lists them) lacks.
pub fn gitlab_missing(scopes: &[String]) -> Vec<&'static Need> {
    GITLAB.iter().filter(|n| !scopes.iter().any(|s| s == n.id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_from_before_lacks_only_what_came_since() {
        let full = aws_runner_policy("123456789012", "abc");
        assert!(aws_runner_missing(Some(&full), "123456789012", "abc").is_empty());
        // 0.9.11's policy: no price list, no CloudWatch, no networks.
        let mut old = full.clone();
        old["Statement"].as_array_mut().unwrap().retain(|s| !["Prices", "MachineTraffic", "ReadNetwork"].contains(&s["Sid"].as_str().unwrap()));
        let missing: Vec<&str> = aws_runner_missing(Some(&old), "123456789012", "abc").iter().map(|n| n.id).collect();
        assert_eq!(missing, ["Prices", "MachineTraffic", "ReadNetwork"]);
        // A statement changed by hand counts as missing; IAM giving one action back as a list of one does not.
        let mut changed = full.clone();
        changed["Statement"][4]["Action"] = json!(["ec2:DescribeImages"]);
        changed["Statement"][5]["Action"] = json!(["pricing:GetProducts"]);
        assert_eq!(aws_runner_missing(Some(&changed), "123456789012", "abc").iter().map(|n| n.id).collect::<Vec<_>>(), ["Read"]);
        // Another control plane's policy is not this one's.
        assert_eq!(aws_runner_missing(Some(&full), "123456789012", "other").len(), 2, "its two statements naming the control plane");
        assert_eq!(aws_runner_missing(None, "123456789012", "abc").len(), AWS_RUNNER.len());
    }

    #[test]
    fn github_and_gitlab_say_what_is_missing() {
        assert!(github_missing(&json!({ "organization_self_hosted_runners": "write", "actions": "write", "metadata": "read" }), true).is_empty());
        // An App from before jobs were run again after a spot interruption: Actions (read) only.
        assert_eq!(github_missing(&json!({ "organization_self_hosted_runners": "write", "actions": "read", "metadata": "read" }), true).iter().map(|n| n.id).collect::<Vec<_>>(), ["actions"]);
        assert_eq!(github_missing(&json!({ "organization_self_hosted_runners": "read", "metadata": "read" }), true).iter().map(|n| n.id).collect::<Vec<_>>(), ["organization_self_hosted_runners", "actions"]);
        assert_eq!(github_missing(&json!({ "administration": "write", "actions": "write", "metadata": "read" }), false).len(), 0);
        assert_eq!(gitlab_missing(&["api".into(), "create_runner".into()]).iter().map(|n| n.id).collect::<Vec<_>>(), ["manage_runner"]);
    }
}
