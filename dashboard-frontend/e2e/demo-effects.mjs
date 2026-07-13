const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Install recording-only motion graphics. Nothing is persisted into the production dashboard. */
export async function installDemoEffects(page, model) {
  await page.evaluate((modelName) => {
    const style = document.createElement('style');
    style.textContent = `
      @keyframes argus-grid-drift { to { background-position: 64px 64px; } }
      @keyframes argus-scan { 0% { transform: translateY(-12vh); opacity: 0; }
        12% { opacity: .22; } 88% { opacity: .16; } 100% { transform: translateY(112vh); opacity: 0; } }
      @keyframes argus-badge-pulse { 0%,100% { box-shadow: 0 0 0 0 rgba(52,211,153,.35); }
        50% { box-shadow: 0 0 0 7px rgba(52,211,153,0); } }
      @keyframes argus-orbit { to { transform: rotate(360deg); } }
      @keyframes argus-orbit-reverse { to { transform: rotate(-360deg); } }
      @keyframes argus-title-in { from { opacity: 0; transform: translateY(24px) scale(.97); filter: blur(8px); }
        to { opacity: 1; transform: translateY(0) scale(1); filter: blur(0); } }
      @keyframes argus-shimmer { from { transform: translateX(-120%); } to { transform: translateX(220%); } }
      @keyframes argus-chapter-in { 0% { opacity: 0; clip-path: inset(0 100% 0 0); }
        24% { opacity: 1; clip-path: inset(0 0 0 0); } 78% { opacity: 1; clip-path: inset(0 0 0 0); }
        100% { opacity: 0; clip-path: inset(0 0 0 100%); } }
      @keyframes argus-chapter-line { from { transform: scaleX(0); } to { transform: scaleX(1); } }
      @keyframes argus-cursor-click { 0% { transform: scale(1); opacity: 1; }
        70% { transform: scale(2.35); opacity: 0; } 100% { transform: scale(2.35); opacity: 0; } }
      @keyframes argus-focus-pulse { 0%,100% { box-shadow: 0 0 0 1px rgba(139,92,246,.65), 0 0 30px rgba(139,92,246,.2); }
        50% { box-shadow: 0 0 0 3px rgba(34,211,238,.75), 0 0 55px rgba(34,211,238,.32); } }

      #argus-fx-grid, #argus-fx-vignette, #argus-fx-scan { position: fixed; z-index: 2147483638;
        inset: 0; pointer-events: none; }
      #argus-fx-grid { opacity: .035; background-image:
        linear-gradient(rgba(139,92,246,.7) 1px, transparent 1px),
        linear-gradient(90deg, rgba(34,211,238,.55) 1px, transparent 1px);
        background-size: 64px 64px; animation: argus-grid-drift 18s linear infinite; }
      #argus-fx-vignette { background: radial-gradient(circle at 50% 45%, transparent 42%, rgba(1,3,12,.33) 100%); }
      #argus-fx-scan { height: 2px; top: 0; bottom: auto;
        background: linear-gradient(90deg, transparent 2%, #8b5cf6 25%, #22d3ee 50%, #34d399 75%, transparent 98%);
        box-shadow: 0 0 18px rgba(34,211,238,.75); animation: argus-scan 7s linear infinite; }

      #argus-demo-badge { position: fixed; z-index: 2147483644; top: 72px; right: 30px;
        display: flex; align-items: center; gap: 8px; padding: 8px 12px;
        border: 1px solid rgba(110,231,183,.55); border-radius: 999px;
        background: linear-gradient(110deg, rgba(8,14,27,.94), rgba(18,18,43,.9)); color: #6ee7b7;
        font: 600 11px/1.2 "IBM Plex Mono", monospace; letter-spacing: .09em;
        box-shadow: 0 8px 30px rgba(0,0,0,.4), inset 0 0 18px rgba(52,211,153,.06); pointer-events: none; }
      #argus-demo-badge::before { content: ''; width: 7px; height: 7px; border-radius: 50%;
        background: #34d399; animation: argus-badge-pulse 1.8s ease-in-out infinite; }

      #argus-demo-caption { position: fixed; z-index: 2147483645; left: 42px; bottom: 34px;
        width: min(700px, calc(100vw - 84px)); padding: 17px 20px 18px; overflow: hidden;
        border: 1px solid rgba(139,92,246,.72); border-radius: 12px;
        background: linear-gradient(125deg, rgba(8,12,24,.96), rgba(20,18,48,.93)); color: #e5e7eb;
        box-shadow: 0 22px 70px rgba(0,0,0,.62), inset 0 1px rgba(255,255,255,.05);
        opacity: 0; transform: translateY(18px) scale(.985); filter: blur(4px);
        transition: opacity .32s ease, transform .32s cubic-bezier(.2,.8,.2,1), filter .32s ease;
        pointer-events: none; }
      #argus-demo-caption::before { content: ''; position: absolute; inset: 0 auto auto 0; width: 42%; height: 2px;
        background: linear-gradient(90deg, transparent, #8b5cf6, #22d3ee, transparent);
        animation: argus-shimmer 1.8s ease-in-out infinite; }
      #argus-demo-caption.visible { opacity: 1; transform: translateY(0) scale(1); filter: blur(0); }
      #argus-demo-caption i { display: block; margin-bottom: 4px; color: #22d3ee; font: 600 9px/1.2 "IBM Plex Mono", monospace;
        font-style: normal; letter-spacing: .18em; text-transform: uppercase; }
      #argus-demo-caption strong { display: block; margin-bottom: 6px;
        background: linear-gradient(90deg, #c4b5fd, #67e8f9); background-clip: text; color: transparent;
        font: 700 21px/1.2 "Space Grotesk", sans-serif; }
      #argus-demo-caption span { display: block; color: #cbd5e1; font: 400 13px/1.48 "IBM Plex Mono", monospace; }

      #argus-demo-slate { position: fixed; z-index: 2147483647; inset: 0; display: flex;
        align-items: center; justify-content: center; overflow: hidden;
        background: radial-gradient(circle at 50% 45%, rgba(42,27,88,.72), rgba(5,8,18,.98) 56%); color: #f8fafc;
        opacity: 0; transition: opacity .38s ease; pointer-events: none; }
      #argus-demo-slate.visible { opacity: 1; }
      #argus-demo-slate .orbit { position: absolute; left: 50%; top: 50%; width: 620px; height: 620px;
        margin: -310px; border: 1px solid rgba(139,92,246,.26); border-radius: 50%;
        animation: argus-orbit 12s linear infinite; }
      #argus-demo-slate .orbit::before, #argus-demo-slate .orbit::after { content: ''; position: absolute;
        width: 9px; height: 9px; border-radius: 50%; box-shadow: 0 0 22px currentColor; }
      #argus-demo-slate .orbit::before { left: 74px; top: 80px; color: #22d3ee; background: #22d3ee; }
      #argus-demo-slate .orbit::after { right: 34px; bottom: 140px; color: #a78bfa; background: #a78bfa; }
      #argus-demo-slate .orbit.second { width: 840px; height: 840px; margin: -420px;
        border-style: dashed; border-color: rgba(34,211,238,.13); animation: argus-orbit-reverse 20s linear infinite; }
      #argus-demo-slate > .content { position: relative; z-index: 1; max-width: 1050px; padding: 52px; text-align: center;
        animation: argus-title-in .65s cubic-bezier(.2,.8,.2,1) both; }
      #argus-demo-slate em { display: block; margin-bottom: 14px; color: #67e8f9;
        font: 600 11px/1.2 "IBM Plex Mono", monospace; font-style: normal; letter-spacing: .28em; text-transform: uppercase; }
      #argus-demo-slate h1 { margin: 0 0 18px;
        background: linear-gradient(100deg, #ddd6fe 15%, #a78bfa 42%, #67e8f9 72%, #6ee7b7);
        background-clip: text; color: transparent; font: 700 58px/1.02 "Space Grotesk", sans-serif;
        letter-spacing: -.035em; text-shadow: 0 0 55px rgba(139,92,246,.18); }
      #argus-demo-slate p { margin: 0; color: #cbd5e1; font: 400 18px/1.5 "IBM Plex Mono", monospace; }

      #argus-fx-chapter { position: fixed; z-index: 2147483646; inset: 0; display: flex; align-items: center;
        padding-left: 12vw; background: linear-gradient(105deg, rgba(5,8,18,.97) 0 46%, rgba(36,23,78,.9) 70%, rgba(6,35,48,.88));
        opacity: 0; pointer-events: none; }
      #argus-fx-chapter.visible { animation: argus-chapter-in 1.15s cubic-bezier(.65,0,.25,1) both; }
      #argus-fx-chapter .number { margin-right: 28px; color: rgba(167,139,250,.34);
        font: 700 112px/1 "Space Grotesk", sans-serif; letter-spacing: -.08em; }
      #argus-fx-chapter .copy { border-left: 2px solid #22d3ee; padding-left: 28px; }
      #argus-fx-chapter strong { display: block; color: #f5f3ff; font: 700 42px/1.05 "Space Grotesk", sans-serif;
        letter-spacing: -.025em; }
      #argus-fx-chapter span { display: block; margin-top: 9px; color: #93c5fd;
        font: 500 12px/1.4 "IBM Plex Mono", monospace; letter-spacing: .12em; text-transform: uppercase; }
      #argus-fx-chapter .line { position: absolute; left: 0; bottom: 0; width: 100%; height: 4px;
        transform-origin: left; background: linear-gradient(90deg, #8b5cf6, #22d3ee, #34d399);
        animation: argus-chapter-line .8s ease-out both; }

      #argus-fx-cursor { position: fixed; z-index: 2147483647; left: 0; top: 0; width: 34px; height: 34px;
        border: 2px solid rgba(103,232,249,.95); border-radius: 50%; opacity: 0;
        box-shadow: 0 0 22px rgba(34,211,238,.65), inset 0 0 10px rgba(139,92,246,.45);
        transform: translate3d(-60px,-60px,0); transition: transform .42s cubic-bezier(.2,.85,.2,1), opacity .2s ease;
        pointer-events: none; }
      #argus-fx-cursor::before { content: ''; position: absolute; width: 4px; height: 4px; left: 13px; top: 13px;
        border-radius: 50%; background: #f8fafc; box-shadow: 0 0 8px #22d3ee; }
      #argus-fx-cursor.visible { opacity: 1; }
      #argus-fx-cursor.click::after { content: ''; position: absolute; inset: -2px; border: 2px solid #a78bfa;
        border-radius: 50%; animation: argus-cursor-click .5s ease-out both; }

      #argus-fx-focus { position: fixed; z-index: 2147483642; opacity: 0; border-radius: 10px;
        border: 1px solid rgba(103,232,249,.75); background: rgba(139,92,246,.018);
        transition: all .36s cubic-bezier(.2,.8,.2,1), opacity .2s ease;
        animation: argus-focus-pulse 2s ease-in-out infinite; pointer-events: none; }
      #argus-fx-focus.visible { opacity: 1; }
    `;
    document.head.append(style);

    const add = (id, className = '') => {
      const node = document.createElement('div');
      node.id = id;
      node.className = className;
      document.body.append(node);
      return node;
    };
    add('argus-fx-grid');
    add('argus-fx-vignette');
    add('argus-fx-scan');

    const badge = add('argus-demo-badge');
    badge.textContent = `LIVE PRODUCTION · ${modelName}`;

    const caption = add('argus-demo-caption');
    caption.append(document.createElement('i'), document.createElement('strong'), document.createElement('span'));

    const slate = add('argus-demo-slate');
    const orbit = document.createElement('div');
    orbit.className = 'orbit';
    const orbit2 = document.createElement('div');
    orbit2.className = 'orbit second';
    const content = document.createElement('div');
    content.className = 'content';
    content.append(document.createElement('em'), document.createElement('h1'), document.createElement('p'));
    slate.append(orbit, orbit2, content);

    const chapter = add('argus-fx-chapter');
    const number = document.createElement('div');
    number.className = 'number';
    const copy = document.createElement('div');
    copy.className = 'copy';
    copy.append(document.createElement('strong'), document.createElement('span'));
    const line = document.createElement('div');
    line.className = 'line';
    chapter.append(number, copy, line);

    add('argus-fx-cursor');
    add('argus-fx-focus');
  }, model);
}

export async function showCaption(page, title, detail, holdMs = 2_800, kicker = 'LIVE TELEMETRY') {
  await page.evaluate(({ captionTitle, captionDetail, captionKicker }) => {
    const root = document.querySelector('#argus-demo-caption');
    if (!(root instanceof HTMLElement)) return;
    const kickerNode = root.querySelector('i');
    const titleNode = root.querySelector('strong');
    const detailNode = root.querySelector('span');
    if (kickerNode) kickerNode.textContent = captionKicker;
    if (titleNode) titleNode.textContent = captionTitle;
    if (detailNode) detailNode.textContent = captionDetail;
    root.classList.add('visible');
  }, { captionTitle: title, captionDetail: detail, captionKicker: kicker });
  await sleep(holdMs);
  await page.evaluate(() => document.querySelector('#argus-demo-caption')?.classList.remove('visible'));
  await sleep(380);
}

export async function showSlate(page, title, detail, holdMs, eyebrow = 'ARGUS · LLMCONDUIT') {
  await page.evaluate(({ slateTitle, slateDetail, slateEyebrow }) => {
    const root = document.querySelector('#argus-demo-slate');
    if (!(root instanceof HTMLElement)) return;
    const eyebrowNode = root.querySelector('em');
    const titleNode = root.querySelector('h1');
    const detailNode = root.querySelector('p');
    if (eyebrowNode) eyebrowNode.textContent = slateEyebrow;
    if (titleNode) titleNode.textContent = slateTitle;
    if (detailNode) detailNode.textContent = slateDetail;
    root.classList.add('visible');
  }, { slateTitle: title, slateDetail: detail, slateEyebrow: eyebrow });
  await sleep(holdMs);
  await page.evaluate(() => document.querySelector('#argus-demo-slate')?.classList.remove('visible'));
  await sleep(550);
}

export async function showChapter(page, number, title, detail) {
  await page.evaluate(({ chapterNumber, chapterTitle, chapterDetail }) => {
    const root = document.querySelector('#argus-fx-chapter');
    if (!(root instanceof HTMLElement)) return;
    root.classList.remove('visible');
    const numberNode = root.querySelector('.number');
    const titleNode = root.querySelector('strong');
    const detailNode = root.querySelector('span');
    if (numberNode) numberNode.textContent = chapterNumber;
    if (titleNode) titleNode.textContent = chapterTitle;
    if (detailNode) detailNode.textContent = chapterDetail;
    void root.offsetWidth;
    root.classList.add('visible');
  }, { chapterNumber: number, chapterTitle: title, chapterDetail: detail });
  await sleep(1_170);
  await page.evaluate(() => document.querySelector('#argus-fx-chapter')?.classList.remove('visible'));
  await sleep(140);
}

export async function fancyClick(page, locator) {
  const box = await locator.boundingBox().catch(() => null);
  if (box) {
    await page.evaluate(({ x, y }) => {
      const cursor = document.querySelector('#argus-fx-cursor');
      if (!(cursor instanceof HTMLElement)) return;
      cursor.classList.add('visible');
      cursor.style.transform = `translate3d(${x - 17}px, ${y - 17}px, 0)`;
    }, { x: box.x + box.width / 2, y: box.y + box.height / 2 });
    await sleep(430);
    await page.evaluate(() => document.querySelector('#argus-fx-cursor')?.classList.add('click'));
    await sleep(230);
  }
  await locator.click();
  await sleep(250);
  await page.evaluate(() => {
    const cursor = document.querySelector('#argus-fx-cursor');
    cursor?.classList.remove('click', 'visible');
  });
}

export async function spotlight(page, locator) {
  const box = await locator.boundingBox().catch(() => null);
  if (!box) return false;
  await page.evaluate(({ x, y, width, height }) => {
    const focus = document.querySelector('#argus-fx-focus');
    if (!(focus instanceof HTMLElement)) return;
    const pad = 7;
    focus.style.left = `${x - pad}px`;
    focus.style.top = `${y - pad}px`;
    focus.style.width = `${width + pad * 2}px`;
    focus.style.height = `${height + pad * 2}px`;
    focus.classList.add('visible');
  }, box);
  return true;
}

export async function clearSpotlight(page) {
  await page.evaluate(() => document.querySelector('#argus-fx-focus')?.classList.remove('visible'));
}
