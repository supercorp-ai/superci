//! Where a control plane runs (the always-on part: GitHub's webhooks in, machines for jobs out). The dashboard finds
//! it in your clouds after you sign in; the one in use is kept with the sign-ins (store.rs), so commands reach it at once.

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Plane {
    Cloudflare { account_id: String, account_name: String, script: String, url: String, plane_id: String, label: String },
    /// A Lambda function with its URL, a DynamoDB table and a schedule (see aws_plane.rs).
    Aws { account_id: String, region: String, url: String, plane_id: String, label: String },
    /// A web endpoint with two Dicts and a schedule in a Modal workspace (see modal_plane.py).
    Modal { workspace: String, url: String, plane_id: String, label: String },
    /// Known only from an AWS account that runs its jobs (sign in where it runs to change it).
    Seen { url: String, plane_id: String },
}

impl Plane {
    pub fn url(&self) -> &str { match self { Plane::Cloudflare { url, .. } | Plane::Aws { url, .. } | Plane::Modal { url, .. } | Plane::Seen { url, .. } => url } }
    pub fn plane_id(&self) -> &str { match self { Plane::Cloudflare { plane_id, .. } | Plane::Aws { plane_id, .. } | Plane::Modal { plane_id, .. } | Plane::Seen { plane_id, .. } => plane_id } }
    /// Its label, as just changed in the dashboard (a control plane known only by its address keeps none here).
    pub fn set_label(&mut self, to: &str) { match self { Plane::Cloudflare { label, .. } | Plane::Aws { label, .. } | Plane::Modal { label, .. } => *label = to.to_string(), Plane::Seen { .. } => {} } }
    pub fn label(&self) -> &str { match self { Plane::Cloudflare { label, .. } | Plane::Aws { label, .. } | Plane::Modal { label, .. } => label, Plane::Seen { .. } => "superci" } }
    pub fn place(&self) -> String {
        match self {
            Plane::Cloudflare { account_name, .. } => format!("Cloudflare · {account_name}"),
            Plane::Aws { account_id, region, .. } => format!("AWS · {account_id} · {region}"),
            Plane::Modal { workspace, .. } => format!("Modal · {workspace}"),
            Plane::Seen { .. } => match self.cloud() { "cloudflare" => "Cloudflare", "modal" => "Modal", _ => "AWS" }.to_string(),
        }
    }

    /// The cloud it runs in (one known only by its address: from the address).
    /// The Modal workspace it runs in, if that is where.
    pub fn workspace(&self) -> Option<&str> { match self { Plane::Modal { workspace, .. } => Some(workspace), _ => None } }

    pub fn cloud(&self) -> &'static str {
        match self {
            Plane::Cloudflare { .. } => "cloudflare",
            Plane::Aws { .. } => "aws",
            Plane::Modal { .. } => "modal",
            Plane::Seen { url, .. } => if url.contains(".workers.dev") { "cloudflare" } else if url.contains(".modal.run") { "modal" } else { "aws" },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each kind of control plane takes a new label and says it; one known only by its address keeps none.
    #[test]
    fn each_control_plane_takes_a_new_label_but_one_known_only_by_its_address() {
        let (url, plane_id, label) = (String::from("https://p.example"), String::from("p1"), String::from("superci"));
        let mut planes = [
            Plane::Cloudflare { account_id: "a".into(), account_name: "A".into(), script: "s".into(), url: url.clone(), plane_id: plane_id.clone(), label: label.clone() },
            Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: url.clone(), plane_id: plane_id.clone(), label: label.clone() },
            Plane::Modal { workspace: "w".into(), url: url.clone(), plane_id: plane_id.clone(), label },
            Plane::Seen { url, plane_id },
        ];
        for p in planes.iter_mut() { p.set_label("fast") }
        assert_eq!(planes.iter().map(Plane::label).collect::<Vec<_>>(), ["fast", "fast", "fast", "superci"]);
    }
}
