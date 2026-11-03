//! The AWS account section beside a control plane whose own account runs jobs: each region it may use on a line of
//! its own with how jobs use it, the CPUs AWS lets it run at once, and the runner image; and the regions Runners
//! offers to add. Read from the dashboard's pages, with the test kit's stand-in answering for AWS as real accounts
//! answered.
mod common;
use common::signed_in;
use superci_testkit::{serve, Served, PLANE};

/// Its control plane's own AWS account runs jobs: AWS is a provider on Runners.
fn own_account(c: &mut superci_core::plane::Config) {
    (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX }));
}

/// A control plane in an account that answers as a classic one did, its jobs on spot in these regions, the first
/// favoured, and on-demand turned off.
fn spot_in(regions: &[&str]) -> Served {
    let served = serve(own_account);
    served.aws.replaying(include_str!("../fixtures/aws/classic/look.jsonl"), &[]);
    let regions = serde_json::to_string(regions).unwrap();
    served.aws.put(&format!("/superci/{PLANE}/AWS_REGIONS"), &regions);
    served.aws.put(&format!("/superci/{PLANE}/ROUTING"), &format!(r#"{{"order":[{{"cloud":"aws","regions":{regions}}},{{"cloud":"aws-on-demand","off":true}}]}}"#));
    served
}

/// A page's part (`page`) once the account was looked at and it says all of `says`: its words and its markup.
fn page_saying(link: &str, page: &str, says: &[&str]) -> (String, String) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, html) = common::part(link, &format!("/?p={page}&fragment=1"));
        let words = common::words(&html);
        if words.contains("AWS account") && !words.contains("Asking AWS") && says.iter().all(|s| words.contains(s)) { return (words, html) }
        assert!(std::time::Instant::now() < end, "{page}: not all of {says:?} in\n{words}");
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// A region jobs go to where the runner image is not: its line says both, with Copy beside it, and no Runner image
/// row repeats it. Once a copy is asked for, the row says it is on its way.
#[test]
fn a_region_in_use_without_the_runner_image_says_so_on_its_line_and_its_copy_on_the_way_in_a_row() {
    let served = spot_in(&["us-east-1", "ca-central-1"]);
    let home = signed_in(&served);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0")]);
    let (words, html) = page_saying(&link, "planes", &["N. Virginia us-east-1 Spot, favoured 96 spot CPUs", "Canada ca-central-1 Spot when cheaper · the runner image is not here Copy into Canada"]);
    // (On-demand is off: no line says it.)
    assert!(!words.contains("on-demand first") && !words.contains("· on-demand"), "{words}");
    assert!(!html.contains("<dt>Runner image</dt>"), "each region's line says where the image is: {words}");
    // Copy into Canada pressed, as AWS answered such a copy.
    served.aws.replaying(include_str!("../fixtures/aws/images/01-create.jsonl"), &[]);
    let (status, _, said) = common::post(&link, "/aws/image", &[("account", "111111111111"), ("region", "ca-central-1"), ("next", "?p=planes")]);
    assert!(status < 400, "{status} {}", common::words(&said));
    // (The account looked at again then, as AWS answered with that copy on its way.)
    served.aws.replaying(include_str!("../fixtures/aws/images/03-list-pending.jsonl"), &[]);
    let (_, html) = page_saying(&link, "planes", &["Runner image", "It is being copied into Canada: about ten minutes. Then it can be added as a region on Runners."]);
    assert!(html.contains("<dt>Runner image</dt>"), "{html}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// The account's copy of the runner image, of an older build than its maker's newest: said in a row of its own beside
/// the regions jobs use, with copying the new one in.
#[test]
fn an_older_copy_of_the_runner_image_is_said_beside_the_regions_in_use() {
    let served = spot_in(&["us-east-1", "us-east-2"]);
    // As AWS answered for a copy in Canada, here in N. Virginia, of an older build.
    served.aws.replaying(include_str!("../fixtures/aws/images/01-create.jsonl"), &[("ca-central-1", "us-east-1")]);
    served.aws.replaying(include_str!("../fixtures/aws/images/05-update-newest.jsonl"), &[("20261006080844", "20250101000000"), ("ca-central-1", "us-east-1")]);
    let home = signed_in(&served);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0")]);
    let (words, html) = page_saying(&link, "planes", &["N. Virginia us-east-1 Spot, favoured", "Your copy in N. Virginia is the build of 1 January 2025. This SuperCI uses the build of"]);
    assert!(html.contains("<dt>Runner image</dt>") && words.contains("Copy the new one in"), "{words}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// A region AWS answered with an error that is no refusal (an internal error), while it answered for the others:
/// said beside the regions as not said, and Runners does not offer it.
#[test]
fn a_region_aws_gave_no_clear_answer_for_is_said_beside_the_regions_and_not_offered_on_runners() {
    let served = spot_in(&["us-east-1", "us-east-2"]);
    served.aws.fail("DescribeVpcs|ec2.eu-west-1.", 500, &superci_testkit::ec2_error("InternalError", "An internal error has occurred"), None);
    let home = signed_in(&served);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0")]);
    let (words, _) = page_saying(&link, "planes", &["Ohio us-east-2 Spot when cheaper", "AWS did not say for Ireland: Refresh asks again."]);
    assert!(!words.contains("are allowed: add more on Runners"), "{words}");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, html) = common::part(&link, "/?p=runners&fragment=1");
        if html.contains(r#"<option value="eu-west-1" disabled>Europe (Ireland) · eu-west-1 · AWS did not say: Refresh under Control plane</option>"#) { break }
        assert!(std::time::Instant::now() < end, "{}", common::words(&html));
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// Jobs set to go to regions the account's organization keeps it out of, all of them: said as none allowed, not as
/// lines for regions it may not use.
#[test]
fn regions_in_use_where_the_account_allows_none_are_said_as_none_allowed() {
    let served = spot_in(&["us-east-1", "us-east-2"]);
    let scp = superci_testkit::ec2_error("UnauthorizedOperation", "You are not authorized to perform this operation. User: arn:aws:sts::111111111111:assumed-role/Admin/someone is not authorized to perform: ec2:DescribeVpcs with an explicit deny in a service control policy: arn:aws:organizations::222222222222:policy/o-example001/service_control_policy/p-example1");
    for r in superci_core::page::REGIONS { served.aws.fail(&format!("DescribeVpcs|ec2.{r}."), 403, &scp, None) }
    let home = signed_in(&served);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0")]);
    let (words, html) = page_saying(&link, "planes", &["Regions None of those SuperCI works in", "By its AWS organization"]);
    assert!(!html.contains(r#"class="acct-regions""#), "{words}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// Spot in two regions: their CPUs at once added up only when AWS said both. With Ohio's not said, the row says the
/// region looked in alone; N. Virginia's own, not among the regions' answers but in the account's look, is still said
/// on its line.
#[test]
fn the_spot_cpus_of_two_regions_in_use_are_added_up_only_when_aws_said_both() {
    let served = spot_in(&["us-east-1", "us-east-2"]);
    let unsaid = superci_testkit::json_error("NoSuchResourceException", "The request failed because the specified service does not exist.");
    // (At first neither said: the regions' answers kept for a while, the look asked again at Refresh.)
    for r in ["us-east-1", "us-east-2"] { served.aws.fail(&format!("L-34B43A08|servicequotas.{r}."), 400, &unsaid, None) }
    let home = signed_in(&served);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0")]);
    let (words, _) = page_saying(&link, "planes", &["N. Virginia us-east-1 Spot, favoured", "CPUs at once 64 on-demand · ? spot"]);
    assert!(!words.contains("96 spot CPUs"), "{words}");
    // N. Virginia's said again, at Refresh; Ohio's still not.
    served.aws.fail_none();
    served.aws.fail("L-34B43A08|servicequotas.us-east-2.", 400, &unsaid, None);
    let (status, _, said) = common::post(&link, "/aws/look", &[("next", "?p=planes")]);
    assert!(status < 400, "{status} {}", common::words(&said));
    let (words, _) = page_saying(&link, "planes", &["N. Virginia us-east-1 Spot, favoured 96 spot CPUs", "CPUs at once 64 on-demand · 96 spot", "in N. Virginia."]);
    assert!(words.contains("Ohio us-east-2 Spot when cheaper Canada") && !words.contains("across N. Virginia, Ohio"), "{words}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// AWS answering no region clearly (it could not be reached): nothing is known of any, so Runners still offers each
/// region rather than none.
#[test]
fn regions_are_still_offered_on_runners_when_aws_answered_for_none_of_them() {
    let served = spot_in(&["us-east-1", "us-east-2"]);
    for r in superci_core::page::REGIONS { served.aws.fail(&format!("DescribeVpcs|ec2.{r}."), 500, &superci_testkit::ec2_error("InternalError", "An internal error has occurred"), None) }
    let home = signed_in(&served);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0")]);
    page_saying(&link, "planes", &["Not known", "AWS did not answer when asked what this account allows"]);
    let (_, html) = common::part(&link, "/?p=runners&fragment=1");
    assert!(html.contains(r#"<option value="eu-west-1">Europe (Ireland) · eu-west-1</option>"#) && !html.contains("AWS did not say: Refresh under Control plane"), "{}", common::words(&html));
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}
