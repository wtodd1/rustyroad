use std::fs::File;

use clap::Parser;
use epub_builder::EpubBuilder;
use epub_builder::EpubContent;
use epub_builder::ReferenceType;
use epub_builder::ZipLibrary;
use eyre::{eyre, Result};
use futures::TryStreamExt;
use futures::{stream, StreamExt};
use reqwest::Url;
use scraper::Html;
use scraper::Selector;

#[derive(Parser, Debug)]
#[command()]
struct Args {
    #[arg(short, long)]
    url: String,

    #[arg(short, long)]
    out: String,

    #[arg(short, long, default_value_t = 5)]
    concurrent: usize,
}

#[derive(Debug, Clone)]
pub struct Chapter {
    name: String,
    link: String,
}

#[derive(Debug)]
pub struct Story {
    title: String,
    author: String,
    description: String,
    cover: String,
    chapters: Vec<Chapter>,
}

fn selector(str: &str) -> Result<Selector> {
    Selector::parse(str).map_err(|_| eyre!("invalid selector"))
}

async fn fetch_story(url: String) -> Result<Story> {
    let url = url.split("/chapter/").next().unwrap();
    let resp = reqwest::get(url).await?.text().await?;

    let doc = Html::parse_document(resp.as_str());

    let cover = doc
        .select(&selector(r#"meta[name="twitter:image"]"#)?)
        .next()
        .unwrap()
        .attr("content")
        .ok_or_else(|| eyre!("could not find cover image"))?
        .to_string();

    let author = doc
        .select(&selector(r#"meta[name="twitter:creator"]"#)?)
        .next()
        .unwrap()
        .attr("content")
        .ok_or_else(|| eyre!("could not find author"))?
        .to_string();

    let title = doc
        .select(&selector(r#"meta[name="twitter:title"]"#)?)
        .next()
        .unwrap()
        .attr("content")
        .ok_or_else(|| eyre!("could not find title"))?
        .to_string();

    let description = doc
        .select(&selector(r#"meta[name="twitter:description"]"#)?)
        .next()
        .unwrap()
        .attr("content")
        .ok_or_else(|| eyre!("could not find description"))?
        .to_string();

    let table = doc
        .select(&selector(r#"table[id="chapters"]"#)?)
        .next()
        .ok_or_else(|| eyre!("could not find chapters"))?;

    let mut chapters = Vec::new();

    let sel_chapter = selector("tbody > tr > td > a")?;
    for chap in table.select(&sel_chapter) {
        let link = chap.attr("href").unwrap();
        let name = chap.text().next().unwrap().trim();

        chapters.push(Chapter {
            name: name.to_string(),
            link: link.to_string(),
        });
    }

    Ok(Story {
        title,
        author,
        description,
        cover,
        chapters,
    })
}

async fn fetch_chapter_content(url: &str) -> Result<String> {
    let base_url = Url::parse("https://www.royalroad.com")?;
    let url = base_url.join(url)?;
    let resp = reqwest::get(url).await?.text().await?;
    let doc = Html::parse_document(resp.as_str());

    let content = doc
        .select(&selector("div.chapter-content")?)
        .next()
        .ok_or(eyre!("couldn't find chapter content"))?;

    Ok(content.html())
}

async fn fetch_and_add_cover(builder: &mut EpubBuilder<ZipLibrary>, url: &str) -> Result<()> {
    let url = Url::parse(url)?;
    let ext = url.path().split(".").last().unwrap().to_owned();

    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        _ => Err(eyre!("unsupported cover format"))?,
    };

    let data = reqwest::get(url).await?.bytes().await?;
    builder.add_cover_image(format!("cover.{}", ext), data.as_ref(), mime)?;

    let cover_page = format!(r#"<img src="cover.{}" />"#, ext);
    builder.add_content(
        EpubContent::new("cover.xhtml", cover_page.as_bytes())
            .title("Cover")
            .reftype(ReferenceType::Cover),
    )?;

    Ok(())
}

fn add_chapter(
    builder: &mut EpubBuilder<ZipLibrary>,
    nr: usize,
    chapter: &Chapter,
    content: &str,
) -> Result<()> {
    let xhtml = format!(
        r#"<?xml version='1.0' encoding='utf-8'?>
            <html xmlns="http://www.w3.org/1999/xhtml">
                <head>
                    <title>{}</title>
                    <meta http-equiv="Content-Type" content="text/html; charset=utf-8"/>
                    <link rel="stylesheet" type="text/css" href="stylesheet.css"/>
                </head>
                <body>
                    {}
                </body>
            </html>
        "#,
        chapter.name, content
    );

    builder.add_content(
        EpubContent::new(format!("chapter_{}.xhtml", nr), xhtml.as_bytes()).title(&chapter.name),
    )?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("error,rustyroad=info"));

    let args = Args::parse();

    log::info!("fetching story...");
    let story = fetch_story(args.url).await?;

    let mut builder = EpubBuilder::new(ZipLibrary::new()?)?;
    builder.set_title(story.title);
    builder.add_author(story.author);
    builder.add_description(story.description);

    builder.stylesheet(
        r#"
            @page {
                margin-bottom: 5pt;
                margin-top: 5pt;
            }
            
            .chapter-inner {
                font-size: 1em;
                line-height: 1.2;
                margin: 0 5pt;
            }

            p {
                text-indent: 1em;
            }
        "#
        .as_bytes(),
    )?;

    // add the cover image
    log::info!("fetching cover...");
    fetch_and_add_cover(&mut builder, &story.cover).await?;

    // build the table of contents
    builder.inline_toc();

    // fetch and add the chapters
    stream::iter(story.chapters.iter().enumerate())
        .map(|(i, chapter)| async move {
            log::info!("fetching chapter {}...", i);

            let content = fetch_chapter_content(&chapter.link).await?;

            Ok::<_, eyre::Error>((i, chapter, content))
        })
        .buffered(args.concurrent)
        .try_for_each(|(i, chapter, content)| {
            std::future::ready(add_chapter(&mut builder, i, chapter, &content))
        })
        .await?;

    log::info!("generating epub...");
    let mut out = File::create(args.out)?;
    builder.generate(&mut out)?;

    Ok(())
}
