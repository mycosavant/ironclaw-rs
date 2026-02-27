// Playwright browser driver for IronClaw.
//
// Launches headless Chromium and accepts JSON commands on stdin (newline-delimited).
// Each command produces a JSON response on stdout (newline-delimited).
//
// Protocol:
//   stdin  → {"action": "navigate", "url": "https://...", ...}\n
//   stdout ← {"ok": true, "data": {"title": "...", "url": "..."}}\n
//          ← {"ok": false, "error": "..."}\n

'use strict';

const { chromium } = require('playwright');
const readline = require('readline');

const COMMAND_TIMEOUT = 30000; // 30s default for page operations
const NAVIGATION_TIMEOUT = 60000; // 60s for navigation

async function main() {
    const browser = await chromium.launch({
        headless: true,
        args: [
            '--disable-gpu',
            // SECURITY: --no-sandbox disables Chromium's own sandbox. This is
            // safe because the browser tools run inside a Docker container
            // (ToolDomain::Container) which provides process isolation.
            // Do NOT run browser tools outside a container.
            '--no-sandbox',
            '--disable-dev-shm-usage',
            '--disable-extensions',
            '--disable-background-networking',
            '--disable-default-apps',
            '--disable-sync',
            '--disable-translate',
            '--metrics-recording-only',
            '--mute-audio',
            '--no-first-run',
        ],
    });

    const context = await browser.newContext({
        userAgent: 'IronClaw/1.0 (Browser Automation)',
        javaScriptEnabled: true,
        ignoreHTTPSErrors: false,
    });

    const page = await context.newPage();

    // Suppress console messages from pages to avoid stdout pollution
    page.on('console', () => {});
    page.on('pageerror', () => {});

    const rl = readline.createInterface({ input: process.stdin });

    function respond(obj) {
        process.stdout.write(JSON.stringify(obj) + '\n');
    }

    for await (const line of rl) {
        if (!line.trim()) continue;

        let cmd;
        try {
            cmd = JSON.parse(line);
        } catch (parseErr) {
            respond({ ok: false, error: `invalid JSON: ${parseErr.message}` });
            continue;
        }

        try {
            let result;

            switch (cmd.action) {
                case 'navigate': {
                    // Defense-in-depth: Rust side validates URLs, but reject
                    // non-HTTPS here too in case of a bypass.
                    if (!cmd.url || !cmd.url.startsWith('https://')) {
                        throw new Error(
                            `only https:// URLs are allowed, got: ${(cmd.url || '').slice(0, 60)}`
                        );
                    }
                    const timeout = cmd.timeout || NAVIGATION_TIMEOUT;
                    await page.goto(cmd.url, {
                        timeout,
                        waitUntil: 'domcontentloaded',
                    });
                    if (cmd.wait_for) {
                        await page.waitForSelector(cmd.wait_for, {
                            timeout: COMMAND_TIMEOUT,
                        });
                    }
                    result = {
                        title: await page.title(),
                        url: page.url(),
                    };
                    break;
                }

                case 'click': {
                    await page.click(cmd.selector, {
                        timeout: cmd.timeout || COMMAND_TIMEOUT,
                    });
                    result = { clicked: cmd.selector };
                    break;
                }

                case 'type': {
                    await page.fill(cmd.selector, cmd.text, {
                        timeout: cmd.timeout || COMMAND_TIMEOUT,
                    });
                    result = { typed: cmd.text.length };
                    break;
                }

                case 'screenshot': {
                    const opts = {
                        fullPage: cmd.full_page || false,
                        type: 'png',
                    };
                    let buf;
                    if (cmd.selector) {
                        const el = await page.locator(cmd.selector);
                        buf = await el.screenshot(opts);
                    } else {
                        buf = await page.screenshot(opts);
                    }
                    result = { base64: buf.toString('base64') };
                    break;
                }

                case 'read_page': {
                    let text;
                    if (cmd.selector) {
                        text = await page
                            .locator(cmd.selector)
                            .innerText({ timeout: COMMAND_TIMEOUT });
                    } else {
                        text = await page.innerText('body', {
                            timeout: COMMAND_TIMEOUT,
                        });
                    }
                    result = { text };
                    break;
                }

                case 'close': {
                    respond({ ok: true, data: {} });
                    await browser.close();
                    process.exit(0);
                    break; // unreachable, but keeps linter happy
                }

                default:
                    throw new Error(`unknown action: ${cmd.action}`);
            }

            respond({ ok: true, data: result });
        } catch (err) {
            respond({
                ok: false,
                error: err.message || String(err),
            });
        }
    }

    // stdin closed — clean up
    await browser.close();
    process.exit(0);
}

main().catch((err) => {
    process.stderr.write(`browser_driver fatal: ${err.message}\n`);
    process.exit(1);
});
