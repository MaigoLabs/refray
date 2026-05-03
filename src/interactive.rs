use std::io::{self, Write};
use std::path::Path;

use anyhow::Result;
use console::style;
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};

#[cfg(test)]
use anyhow::bail;
#[cfg(test)]
use std::io::BufRead;

use crate::config::{
    Config, EndpointConfig, MirrorConfig, NamespaceKind, ProviderKind, SiteConfig, TokenConfig,
    Visibility,
};
use crate::provider::ProviderClient;

pub fn run_config_wizard(path: &Path) -> Result<()> {
    let mut config = Config::load_or_default(path)?;
    let theme = ColorfulTheme::default();

    println!();
    println!("{}", style("git-sync configuration wizard").cyan().bold());
    println!(
        "{}",
        style("Use arrow keys to select options. Press Enter to accept defaults.").dim()
    );
    println!();

    prompt_sites_styled(&mut config, &theme)?;
    prompt_mirrors_styled(&mut config, &theme)?;

    config.save(path)?;
    println!(
        "{} {}",
        style("saved").green().bold(),
        style(path.display()).cyan()
    );
    Ok(())
}

fn prompt_sites_styled(config: &mut Config, theme: &ColorfulTheme) -> Result<()> {
    loop {
        let default = should_add_site_by_default(config);
        let prompt = if config.sites.is_empty() {
            "Add a site?"
        } else {
            "Add another site?"
        };
        if !Confirm::with_theme(theme)
            .with_prompt(prompt)
            .default(default)
            .interact()?
        {
            break;
        }

        println!("{}", style("Site").magenta().bold());
        let provider = prompt_provider_styled(theme)?;
        let name = Input::<String>::with_theme(theme)
            .with_prompt("Site name")
            .default(default_site_name(config, &provider))
            .validate_with(|value: &String| validate_required(value))
            .interact_text()?;
        let base_url = Input::<String>::with_theme(theme)
            .with_prompt("Base URL")
            .default(default_base_url(&provider).to_string())
            .interact_text()?;
        let api_url = optional_input(theme, "API URL override")?;
        let token = prompt_token_styled(theme, &provider, &base_url, api_url.as_deref())?;
        let git_username = optional_input(theme, "Git username override")?;

        config.upsert_site(SiteConfig {
            name,
            provider,
            base_url,
            api_url,
            token,
            git_username,
        });

        println!("{}", style("Site saved").green());
        println!();
    }
    Ok(())
}

fn prompt_mirrors_styled(config: &mut Config, theme: &ColorfulTheme) -> Result<()> {
    if config.sites.len() < 2 {
        println!(
            "{}",
            style("At least two sites are needed before adding a mirror group.").yellow()
        );
        return Ok(());
    }

    loop {
        let default = config.mirrors.is_empty();
        let prompt = if config.mirrors.is_empty() {
            "Add a mirror group?"
        } else {
            "Add another mirror group?"
        };
        if !Confirm::with_theme(theme)
            .with_prompt(prompt)
            .default(default)
            .interact()?
        {
            break;
        }

        println!("{}", style("Mirror group").magenta().bold());
        let name = Input::<String>::with_theme(theme)
            .with_prompt("Name")
            .validate_with(|value: &String| validate_required(value))
            .interact_text()?;
        let mut endpoints = Vec::new();
        loop {
            endpoints.push(prompt_endpoint_styled(config, theme)?);
            if endpoints.len() >= 2
                && !Confirm::with_theme(theme)
                    .with_prompt("Add another endpoint to this group?")
                    .default(false)
                    .interact()?
            {
                break;
            }
        }

        let create_missing = Confirm::with_theme(theme)
            .with_prompt("Create missing repositories?")
            .default(true)
            .interact()?;
        let visibility = prompt_visibility_styled(theme)?;
        let allow_force = Confirm::with_theme(theme)
            .with_prompt("Allow force-push for diverged branches?")
            .default(false)
            .interact()?;

        config.upsert_mirror(MirrorConfig {
            name,
            endpoints,
            create_missing,
            visibility,
            allow_force,
        });

        println!("{}", style("Mirror group saved").green());
        println!();
    }
    Ok(())
}

fn prompt_provider_styled(theme: &ColorfulTheme) -> Result<ProviderKind> {
    let options = ["GitHub", "GitLab", "Gitea"];
    let index = Select::with_theme(theme)
        .with_prompt("Provider")
        .items(options)
        .default(0)
        .interact()?;
    Ok(match index {
        0 => ProviderKind::Github,
        1 => ProviderKind::Gitlab,
        _ => ProviderKind::Gitea,
    })
}

fn prompt_endpoint_styled(config: &Config, theme: &ColorfulTheme) -> Result<EndpointConfig> {
    let site_names = config
        .sites
        .iter()
        .map(|site| site.name.as_str())
        .collect::<Vec<_>>();
    let site_index = Select::with_theme(theme)
        .with_prompt("Endpoint site")
        .items(&site_names)
        .default(0)
        .interact()?;
    let kind = prompt_namespace_kind_styled(theme)?;
    let namespace = Input::<String>::with_theme(theme)
        .with_prompt("Namespace/account/org/group")
        .validate_with(|value: &String| validate_required(value))
        .interact_text()?;

    Ok(EndpointConfig {
        site: site_names[site_index].to_string(),
        kind,
        namespace,
    })
}

fn prompt_namespace_kind_styled(theme: &ColorfulTheme) -> Result<NamespaceKind> {
    let options = ["User", "Organization", "Group"];
    let index = Select::with_theme(theme)
        .with_prompt("Namespace kind")
        .items(options)
        .default(0)
        .interact()?;
    Ok(match index {
        0 => NamespaceKind::User,
        1 => NamespaceKind::Org,
        _ => NamespaceKind::Group,
    })
}

fn prompt_visibility_styled(theme: &ColorfulTheme) -> Result<Visibility> {
    let options = ["Private", "Public"];
    let index = Select::with_theme(theme)
        .with_prompt("Visibility for created repos")
        .items(options)
        .default(0)
        .interact()?;
    Ok(if index == 0 {
        Visibility::Private
    } else {
        Visibility::Public
    })
}

fn prompt_token_styled(
    theme: &ColorfulTheme,
    provider: &ProviderKind,
    base_url: &str,
    api_url: Option<&str>,
) -> Result<TokenConfig> {
    println!(
        "{}",
        style("The PAT is stored in the config file, which git-sync writes with user-only permissions.").yellow()
    );
    loop {
        let token = Password::with_theme(theme)
            .with_prompt("PAT token")
            .validate_with(|value: &String| validate_required(value))
            .interact()?;
        let token_config = TokenConfig::Value(token);
        let site = SiteConfig {
            name: "validation".to_string(),
            provider: provider.clone(),
            base_url: base_url.to_string(),
            api_url: api_url.map(ToString::to_string),
            token: token_config.clone(),
            git_username: None,
        };

        print!("{}", style("Checking PAT... ").dim());
        io::stdout().flush()?;
        match validate_site_token(&site) {
            Ok(()) => {
                println!("{}", style("valid").green().bold());
                return Ok(token_config);
            }
            Err(error) => {
                println!("{}", style("failed").red().bold());
                println!("{} {error:#}", style("PAT validation error:").red());
                if Confirm::with_theme(theme)
                    .with_prompt("Use this token anyway?")
                    .default(false)
                    .interact()?
                {
                    return Ok(token_config);
                }
            }
        }
    }
}

fn validate_site_token(site: &SiteConfig) -> Result<()> {
    ProviderClient::new(site)?.validate_token()
}

fn optional_input(theme: &ColorfulTheme, prompt: &str) -> Result<Option<String>> {
    let value = Input::<String>::with_theme(theme)
        .with_prompt(prompt)
        .allow_empty(true)
        .interact_text()?;
    Ok((!value.trim().is_empty()).then(|| value.trim().to_string()))
}

fn validate_required(value: &str) -> std::result::Result<(), String> {
    if value.trim().is_empty() {
        Err("A value is required".to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub fn run_config_wizard_with_io<R, W>(
    mut config: Config,
    reader: &mut R,
    writer: &mut W,
) -> Result<Config>
where
    R: BufRead,
    W: Write,
{
    writeln!(writer, "git-sync configuration wizard")?;
    writeln!(writer, "Press Enter to accept defaults shown in brackets.")?;

    prompt_sites(&mut config, reader, writer)?;
    prompt_mirrors(&mut config, reader, writer)?;

    Ok(config)
}

#[cfg(test)]
fn prompt_sites<R, W>(config: &mut Config, reader: &mut R, writer: &mut W) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    loop {
        let default = should_add_site_by_default(config);
        if !prompt_bool(
            reader,
            writer,
            if config.sites.is_empty() {
                "Add a site?"
            } else {
                "Add another site?"
            },
            default,
        )? {
            break;
        }

        let provider = prompt_provider(reader, writer)?;
        let name = prompt_with_default(
            reader,
            writer,
            "Site name",
            &default_site_name(config, &provider),
        )?;
        let base_url =
            prompt_with_default(reader, writer, "Base URL", default_base_url(&provider))?;
        let api_url = prompt_optional(reader, writer, "API URL override")?;
        let token = prompt_token(reader, writer)?;
        let git_username = prompt_optional(reader, writer, "Git username override")?;

        config.upsert_site(SiteConfig {
            name,
            provider,
            base_url,
            api_url,
            token,
            git_username,
        });
    }

    Ok(())
}

#[cfg(test)]
fn prompt_mirrors<R, W>(config: &mut Config, reader: &mut R, writer: &mut W) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    if config.sites.len() < 2 {
        writeln!(
            writer,
            "At least two sites are needed before adding a mirror group."
        )?;
        return Ok(());
    }

    loop {
        let default = config.mirrors.is_empty();
        if !prompt_bool(
            reader,
            writer,
            if config.mirrors.is_empty() {
                "Add a mirror group?"
            } else {
                "Add another mirror group?"
            },
            default,
        )? {
            break;
        }

        let name = prompt_required(reader, writer, "Mirror group name")?;
        let mut endpoints = Vec::new();
        loop {
            writeln!(writer, "Available sites: {}", site_names(config))?;
            let endpoint = prompt_endpoint(config, reader, writer)?;
            endpoints.push(endpoint);

            if endpoints.len() >= 2
                && !prompt_bool(reader, writer, "Add another endpoint to this group?", false)?
            {
                break;
            }
        }

        let create_missing = prompt_bool(reader, writer, "Create missing repositories?", true)?;
        let visibility = prompt_visibility(reader, writer)?;
        let allow_force = prompt_bool(
            reader,
            writer,
            "Allow force-push for diverged branches?",
            false,
        )?;

        config.upsert_mirror(MirrorConfig {
            name,
            endpoints,
            create_missing,
            visibility,
            allow_force,
        });
    }

    Ok(())
}

#[cfg(test)]
fn prompt_endpoint<R, W>(config: &Config, reader: &mut R, writer: &mut W) -> Result<EndpointConfig>
where
    R: BufRead,
    W: Write,
{
    let site = loop {
        let site = prompt_required(reader, writer, "Endpoint site name")?;
        if config.site(&site).is_some() {
            break site;
        }
        writeln!(writer, "Unknown site '{site}'.")?;
    };
    let kind = prompt_namespace_kind(reader, writer)?;
    let namespace = prompt_required(reader, writer, "Namespace/account/org/group")?;

    Ok(EndpointConfig {
        site,
        kind,
        namespace,
    })
}

#[cfg(test)]
fn prompt_provider<R, W>(reader: &mut R, writer: &mut W) -> Result<ProviderKind>
where
    R: BufRead,
    W: Write,
{
    loop {
        let value =
            prompt_with_default(reader, writer, "Provider (github/gitlab/gitea)", "github")?;
        match value.to_ascii_lowercase().as_str() {
            "github" => return Ok(ProviderKind::Github),
            "gitlab" => return Ok(ProviderKind::Gitlab),
            "gitea" => return Ok(ProviderKind::Gitea),
            _ => writeln!(writer, "Provider must be github, gitlab, or gitea.")?,
        }
    }
}

#[cfg(test)]
fn prompt_namespace_kind<R, W>(reader: &mut R, writer: &mut W) -> Result<NamespaceKind>
where
    R: BufRead,
    W: Write,
{
    loop {
        let value = prompt_with_default(reader, writer, "Namespace kind (user/org/group)", "user")?;
        match value.to_ascii_lowercase().as_str() {
            "user" => return Ok(NamespaceKind::User),
            "org" | "organization" => return Ok(NamespaceKind::Org),
            "group" => return Ok(NamespaceKind::Group),
            _ => writeln!(writer, "Namespace kind must be user, org, or group.")?,
        }
    }
}

#[cfg(test)]
fn prompt_visibility<R, W>(reader: &mut R, writer: &mut W) -> Result<Visibility>
where
    R: BufRead,
    W: Write,
{
    loop {
        let value = prompt_with_default(
            reader,
            writer,
            "Visibility for created repos (private/public)",
            "private",
        )?;
        match value.to_ascii_lowercase().as_str() {
            "private" => return Ok(Visibility::Private),
            "public" => return Ok(Visibility::Public),
            _ => writeln!(writer, "Visibility must be private or public.")?,
        }
    }
}

#[cfg(test)]
fn prompt_token<R, W>(reader: &mut R, writer: &mut W) -> Result<TokenConfig>
where
    R: BufRead,
    W: Write,
{
    writeln!(
        writer,
        "The PAT is stored in the config file with user-only permissions."
    )?;
    Ok(TokenConfig::Value(prompt_required(
        reader,
        writer,
        "PAT token",
    )?))
}

#[cfg(test)]
fn prompt_bool<R, W>(reader: &mut R, writer: &mut W, label: &str, default: bool) -> Result<bool>
where
    R: BufRead,
    W: Write,
{
    let default_label = if default { "Y/n" } else { "y/N" };
    loop {
        write!(writer, "{label} [{default_label}]: ")?;
        writer.flush()?;
        let value = read_line(reader)?.trim().to_ascii_lowercase();
        match value.as_str() {
            "" => return Ok(default),
            "y" | "yes" | "true" => return Ok(true),
            "n" | "no" | "false" => return Ok(false),
            _ => writeln!(writer, "Enter yes or no.")?,
        }
    }
}

#[cfg(test)]
fn prompt_required<R, W>(reader: &mut R, writer: &mut W, label: &str) -> Result<String>
where
    R: BufRead,
    W: Write,
{
    loop {
        write!(writer, "{label}: ")?;
        writer.flush()?;
        let value = read_line(reader)?.trim().to_string();
        if !value.is_empty() {
            return Ok(value);
        }
        writeln!(writer, "A value is required.")?;
    }
}

#[cfg(test)]
fn prompt_optional<R, W>(reader: &mut R, writer: &mut W, label: &str) -> Result<Option<String>>
where
    R: BufRead,
    W: Write,
{
    write!(writer, "{label} [none]: ")?;
    writer.flush()?;
    let value = read_line(reader)?.trim().to_string();
    Ok((!value.is_empty()).then_some(value))
}

#[cfg(test)]
fn prompt_with_default<R, W>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    default: &str,
) -> Result<String>
where
    R: BufRead,
    W: Write,
{
    write!(writer, "{label} [{default}]: ")?;
    writer.flush()?;
    let value = read_line(reader)?.trim().to_string();
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value)
    }
}

#[cfg(test)]
fn read_line<R>(reader: &mut R) -> Result<String>
where
    R: BufRead,
{
    let mut value = String::new();
    let bytes = reader.read_line(&mut value)?;
    if bytes == 0 {
        bail!("unexpected end of input while reading interactive configuration");
    }
    Ok(value)
}

fn default_base_url(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Github => "https://github.com",
        ProviderKind::Gitlab => "https://gitlab.com",
        ProviderKind::Gitea => "https://gitea.example.com",
    }
}

fn should_add_site_by_default(config: &Config) -> bool {
    config.sites.len() < 2
}

fn default_site_name(config: &Config, provider: &ProviderKind) -> String {
    let base = match provider {
        ProviderKind::Github => "github",
        ProviderKind::Gitlab => "gitlab",
        ProviderKind::Gitea => "gitea",
    };
    if config.site(base).is_none() {
        return base.to_string();
    }

    for suffix in 2.. {
        let candidate = format!("{base}-{suffix}");
        if config.site(&candidate).is_none() {
            return candidate;
        }
    }
    unreachable!("unbounded suffix search should always return")
}

#[cfg(test)]
fn site_names(config: &Config) -> String {
    config
        .sites
        .iter()
        .map(|site| site.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn wizard_builds_sites_and_mirror_group() {
        let input = [
            "",                           // Add a site? yes
            "",                           // Provider default github
            "",                           // Site name default github
            "",                           // Base URL default
            "",                           // API URL override none
            "gh-token",                   // PAT token
            "",                           // Git username override none
            "",                           // Add another site? yes by default for second site
            "gitea",                      // Provider
            "",                           // Site name default gitea
            "https://gitea.example.test", // Base URL
            "",                           // API URL override none
            "gt-token",                   // PAT token
            "",                           // Git username override none
            "n",                          // Stop adding sites
            "",                           // Add mirror group? yes
            "personal",                   // Mirror group name
            "github",                     // First endpoint site
            "",                           // Namespace kind default user
            "hykilpikonna",               // Namespace
            "gitea",                      // Second endpoint site
            "",                           // Namespace kind default user
            "azalea",                     // Namespace
            "n",                          // Stop adding endpoints
            "",                           // Create missing default yes
            "",                           // Visibility default private
            "",                           // Allow force default no
            "n",                          // Stop adding mirrors
        ]
        .join("\n")
            + "\n";
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();

        let config =
            run_config_wizard_with_io(Config::default(), &mut reader, &mut output).unwrap();

        assert_eq!(config.sites.len(), 2);
        assert_eq!(config.sites[0].name, "github");
        assert_eq!(config.sites[0].provider, ProviderKind::Github);
        assert_eq!(config.sites[0].base_url, "https://github.com");
        assert_eq!(
            config.sites[0].token,
            TokenConfig::Value("gh-token".to_string())
        );
        assert_eq!(config.sites[1].name, "gitea");
        assert_eq!(config.sites[1].provider, ProviderKind::Gitea);
        assert_eq!(config.sites[1].base_url, "https://gitea.example.test");
        assert_eq!(
            config.sites[1].token,
            TokenConfig::Value("gt-token".to_string())
        );

        assert_eq!(config.mirrors.len(), 1);
        assert_eq!(config.mirrors[0].name, "personal");
        assert_eq!(config.mirrors[0].endpoints.len(), 2);
        assert_eq!(config.mirrors[0].endpoints[0].site, "github");
        assert_eq!(config.mirrors[0].endpoints[0].kind, NamespaceKind::User);
        assert_eq!(config.mirrors[0].endpoints[0].namespace, "hykilpikonna");
        assert_eq!(config.mirrors[0].endpoints[1].site, "gitea");
        assert_eq!(config.mirrors[0].endpoints[1].namespace, "azalea");
        assert!(config.mirrors[0].create_missing);
        assert_eq!(config.mirrors[0].visibility, Visibility::Private);
        assert!(!config.mirrors[0].allow_force);

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("git-sync configuration wizard"));
        assert!(output.contains("Available sites: github, gitea"));
    }

    #[test]
    fn wizard_can_update_existing_config_without_adding_anything() {
        let config = Config {
            sites: vec![SiteConfig {
                name: "github".to_string(),
                provider: ProviderKind::Github,
                base_url: "https://github.com".to_string(),
                api_url: None,
                token: TokenConfig::Env("GITHUB_TOKEN".to_string()),
                git_username: None,
            }],
            mirrors: Vec::new(),
        };
        let mut reader = Cursor::new(b"n\n".as_slice());
        let mut output = Vec::new();

        let updated = run_config_wizard_with_io(config.clone(), &mut reader, &mut output).unwrap();

        assert_eq!(updated.sites, config.sites);
        assert!(updated.mirrors.is_empty());
    }

    #[test]
    fn wizard_reports_eof_instead_of_looping() {
        let mut reader = Cursor::new(b"".as_slice());
        let mut output = Vec::new();

        let err = run_config_wizard_with_io(Config::default(), &mut reader, &mut output)
            .unwrap_err()
            .to_string();

        assert!(err.contains("unexpected end of input"));
    }

    #[test]
    fn wizard_defaults_to_second_site_then_stops_after_two_sites() {
        let input = [
            "",         // Add a site? yes
            "",         // Provider default github
            "",         // Site name default github
            "",         // Base URL default
            "",         // API URL override none
            "gh-token", // PAT token
            "",         // Git username override none
            "",         // Add another site? yes by default
            "gitea",    // Provider
            "",         // Site name default gitea
            "",         // Base URL default
            "",         // API URL override none
            "gt-token", // PAT token
            "",         // Git username override none
            "",         // Add another site? no by default after two sites
            "n",        // Add mirror group? no for this focused test
        ]
        .join("\n")
            + "\n";
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();

        let config =
            run_config_wizard_with_io(Config::default(), &mut reader, &mut output).unwrap();

        assert_eq!(config.sites.len(), 2);
        assert_eq!(config.sites[0].name, "github");
        assert_eq!(config.sites[1].name, "gitea");
        assert!(config.mirrors.is_empty());
    }

    #[test]
    fn default_site_names_are_provider_based_and_unique() {
        let mut config = Config::default();
        assert_eq!(default_site_name(&config, &ProviderKind::Github), "github");

        config.upsert_site(SiteConfig {
            name: "github".to_string(),
            provider: ProviderKind::Github,
            base_url: "https://github.com".to_string(),
            api_url: None,
            token: TokenConfig::Value("token".to_string()),
            git_username: None,
        });
        assert_eq!(
            default_site_name(&config, &ProviderKind::Github),
            "github-2"
        );

        config.upsert_site(SiteConfig {
            name: "github-2".to_string(),
            provider: ProviderKind::Github,
            base_url: "https://github.example.test".to_string(),
            api_url: None,
            token: TokenConfig::Value("token".to_string()),
            git_username: None,
        });
        assert_eq!(
            default_site_name(&config, &ProviderKind::Github),
            "github-3"
        );
    }
}
