use std::fs::File;

use clap::Parser;
use epub_builder::EpubBuilder;
use epub_builder::EpubContent;
use epub_builder::ZipLibrary;
use eyre::{eyre, Result};
use futures::TryStreamExt;
use futures::{stream, StreamExt};
use reqwest::header::CONTENT_TYPE;
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
    let resp = reqwest::get(url).await?.text().await?;

    let doc = Html::parse_document(resp.as_str());

    let cover = doc
        .select(&selector(r#"meta[name="twitter:image"]"#)?)
        .next()
        .unwrap()
        .attr("content")
        .unwrap()
        .to_string();

    let author = doc
        .select(&selector(r#"meta[name="twitter:creator"]"#)?)
        .next()
        .unwrap()
        .attr("content")
        .unwrap()
        .to_string();

    let title = doc
        .select(&selector(r#"meta[name="twitter:title"]"#)?)
        .next()
        .unwrap()
        .attr("content")
        .unwrap()
        .to_string();

    let description = doc
        .select(&selector(r#"meta[name="twitter:description"]"#)?)
        .next()
        .unwrap()
        .attr("content")
        .unwrap()
        .to_string();

    let table = doc
        .select(&selector(r#"table[id="chapters"]"#)?)
        .next()
        .unwrap();

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
    builder.inline_toc();

    // add the cover image
    {
        log::info!("fetching cover...");

        let cover = reqwest::get(story.cover).await?;
        let mime = cover
            .headers()
            .get(CONTENT_TYPE)
            .unwrap()
            .to_str()?
            .to_string();
        let data = cover.bytes().await?;
        builder.add_cover_image("cover.png", data.as_ref(), mime)?;
    }

    stream::iter(story.chapters.iter().enumerate())
        .map(|(i, chapter)| async move {
            log::info!("fetching chapter {}...", i);

            let content = fetch_chapter_content(&chapter.link).await?;

            Ok::<_, eyre::Error>((i, chapter, content))
        })
        .buffered(args.concurrent)
        .try_for_each(|(i, chapter, content)| {
            std::future::ready(
                builder
                    .add_content(
                        EpubContent::new(format!("chapter_{}.xhtml", i + 1), content.as_bytes())
                            .title(&chapter.name),
                    )
                    .map(|_| ()),
            )
        })
        .await?;

    log::info!("generating epub...");
    let mut out = File::create(args.out)?;
    builder.generate(&mut out)?;

    Ok(())
}
