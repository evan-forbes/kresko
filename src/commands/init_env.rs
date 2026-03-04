use anyhow::Result;

use crate::config::Provider;

const DO_ENV_TEMPLATE: &str = r#"# DigitalOcean Configuration
DIGITALOCEAN_TOKEN=

# SSH Configuration
KRESKO_SSH_KEY_NAME=your-username
KRESKO_SSH_PUB_KEY_PATH=~/.ssh/id_rsa.pub
KRESKO_SSH_KEY_PATH=~/.ssh/id_rsa

# S3 Configuration (DigitalOcean Spaces or AWS S3)
AWS_ACCESS_KEY_ID=
AWS_SECRET_ACCESS_KEY=
AWS_DEFAULT_REGION=nyc3
AWS_S3_BUCKET=kresko-data
AWS_S3_ENDPOINT=https://nyc3.digitaloceanspaces.com
"#;

const GCP_ENV_TEMPLATE: &str = r#"# Google Cloud Configuration
GOOGLE_CLOUD_PROJECT=
GOOGLE_CLOUD_KEY_JSON_PATH=

# SSH Configuration
KRESKO_SSH_KEY_NAME=your-username
KRESKO_SSH_PUB_KEY_PATH=~/.ssh/id_rsa.pub
KRESKO_SSH_KEY_PATH=~/.ssh/id_rsa

# S3 Configuration (GCS or AWS S3)
AWS_ACCESS_KEY_ID=
AWS_SECRET_ACCESS_KEY=
AWS_DEFAULT_REGION=us-east-1
AWS_S3_BUCKET=kresko-data
AWS_S3_ENDPOINT=
"#;

pub fn run(provider: &str) -> Result<()> {
    let provider: Provider = provider.parse()?;
    let template = match provider {
        Provider::DigitalOcean => DO_ENV_TEMPLATE,
        Provider::GoogleCloud => GCP_ENV_TEMPLATE,
    };

    let path = std::path::Path::new(".env");
    if path.exists() {
        anyhow::bail!(".env file already exists. Remove it first if you want to regenerate.");
    }

    std::fs::write(path, template)?;
    println!("Generated .env template for {provider}");
    println!("Edit .env and fill in your credentials before running other commands.");

    Ok(())
}
