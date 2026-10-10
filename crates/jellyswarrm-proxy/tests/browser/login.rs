use super::*;

#[tokio::test]
#[ignore = "requires Chrome (or the Docker browser image in container mode)"]
async fn manual_login_handles_automatic_form_transition() -> Result<()> {
    let playwright = Playwright::launch().await?;
    let container = if std::env::var_os("JELLYSWARRM_BROWSER_CONTAINERS").is_some() {
        Some(containers::ContainerBrowser::start().await?)
    } else {
        None
    };
    let options = LaunchOptions::default().headless(true);
    let options = match std::env::var("JELLYSWARRM_CHROME_PATH") {
        Ok(path) => options.executable_path(path),
        Err(_) => options.channel("chrome".to_owned()),
    };
    let browser = containers::launch(&playwright, options, container.as_ref()).await?;
    let result = async {
        let context = browser.new_context().await?;
        for mode in ["already-open", "button", "automatic"] {
            let page = context.new_page().await?;
            page.set_default_timeout(3_000.0).await;
            page.set_content(&format!(r#"
                <style>
                    @keyframes moving {{ from {{ transform: translateX(0); }} to {{ transform: translateX(20px); }} }}
                    .btnManual {{ animation: moving .2s infinite alternate; }}
                </style>
                <input id="txtManualName" style="display:none">
                <button class="btnManual">Manual login</button>
                <script>
                    const input = document.querySelector('#txtManualName');
                    const button = document.querySelector('.btnManual');
                    const showForm = () => {{ input.style.display = 'block'; button.style.display = 'none'; }};
                    if ('{mode}' === 'already-open') showForm();
                    else if ('{mode}' === 'button') button.onclick = showForm;
                    else setTimeout(showForm, 250);
                </script>
            "#), None).await?;
            // The moving button holds a normal Playwright click in its
            // actionability wait while the automatic case hides it. The old
            // helper timed out instead of noticing the now-visible form.
            open_manual_login(&page).await.with_context(|| format!("manual login mode: {mode}"))?;
            page.locator("#txtManualName:visible").fill(USERNAME, None).await?;
            anyhow::ensure!(page.locator("#txtManualName").input_value(None).await? == USERNAME);
            page.close().await?;
        }
        context.close().await?;
        Ok::<(), anyhow::Error>(())
    }.await;
    let closed = browser.close().await;
    result?;
    closed?;
    Ok(())
}
