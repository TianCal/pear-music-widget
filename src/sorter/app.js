(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const $ = (id) => document.getElementById(id);

  const HOW = {
    title: "副歌 · 按歌名定位",
    lyrics: "副歌 · 歌词重复段",
    third: "副歌 · 长重复段",
    fallback: "没有时间轴歌词 · 从 35% 处开始",
  };

  let view = null;
  let covers = {};
  let choruses = {};
  /** id → lyrics, `null` for none found. Absent while still looking. */
  let lyricsById = {};
  /** Line start times and centres of the rendered lyrics, for the roll. */
  let rolled = { times: [], centres: [], active: -1, offset: 0 };
  /** Wheel scrolling pauses the roll until then (ms since epoch). */
  let manualUntil = 0;
  const MANUAL_MS = 4000;
  let dragging = false;
  /** A rating sent and its card not yet replaced: ignore further input. */
  let busy = false;
  /** Resolves when the outgoing card has finished leaving. */
  let leaving = Promise.resolve();

  const clock = (s) => {
    s = Math.max(0, Math.floor(s || 0));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
  };
  let lastPosition = 0;
  const basename = (p) => (p || "").split("/").filter(Boolean).pop() || p;

  function toast(text) {
    const t = $("toast");
    t.textContent = text;
    t.hidden = false;
    clearTimeout(toast.timer);
    toast.timer = setTimeout(() => (t.hidden = true), 4000);
  }

  // ------------------------------------------------------------- rendering

  function showEmpty(glyph, title, detail, action) {
    $("card").hidden = true;
    $("empty").hidden = false;
    $("empty-glyph").textContent = glyph;
    $("empty-title").textContent = title;
    $("empty-detail").textContent = detail;
    $("empty-action").textContent = action.label;
    $("empty-action").onclick = action.run;
    $("empty-action").className = "push" + (action.primary ? " default" : "");
  }

  function renderStatus() {
    $("folder-name").textContent = view?.folder ? basename(view.folder) : "选择文件夹";
    $("folder").title = view?.folder || "选择音乐文件夹";
    if (!view?.folder) {
      $("stats").textContent = "";
    } else {
      const done = view.total - view.remaining;
      $("stats").textContent = `已筛 ${done} / ${view.total} · 喜欢 ${view.liked} · 不喜欢 ${view.disliked}`;
    }
    $("review").disabled = !view?.disliked;
    $("reset").disabled = !(view?.liked || view?.disliked);
    $("review").textContent = view?.disliked ? `清理不喜欢的 (${view.disliked})…` : "清理不喜欢的…";
    $("undo").disabled = !view?.canUndo;
    $("skip").disabled = !view?.card || view.remaining < 2;
  }

  function renderChorus() {
    const card = view?.card;
    if (!card) return;
    const c = choruses[card.id] || card.chorus;
    const d = card.duration || 1;
    if (!c) {
      $("chorus-label").textContent = "正在找副歌…";
      $("clip").style.width = "0";
      return;
    }
    $("chorus-label").textContent = `${HOW[c.how] || "副歌"} · ${clock(c.start)}`;
    $("clip").style.left = `${(c.start / d) * 100}%`;
    $("clip").style.width = `${(Math.min(15, d - c.start) / d) * 100}%`;
    markChorusLines();
  }

  function markChorusLines() {
    const card = view?.card;
    const c = card && (choruses[card.id] || card.chorus);
    $("lyrics-lines").querySelectorAll("button").forEach((b) => {
      const t = Number(b.dataset.time);
      b.classList.toggle("chorus", !!c && b.dataset.time !== undefined && t >= c.start && t < c.start + 15);
    });
  }

  function renderLyrics() {
    const card = view?.card;
    const pane = $("lyrics");
    const box = $("lyrics-lines");
    pane.querySelector(".note")?.remove();
    box.replaceChildren();
    box.className = "lyrics-lines";
    box.style.transform = "";
    rolled = { times: [], centres: [], active: -1, offset: 0 };
    manualUntil = 0;
    if (!card) return;
    const words = lyricsById[card.id];
    if (words === undefined || words === null || !words.lines?.length) {
      const note = document.createElement("div");
      note.className = "note";
      note.textContent = words === undefined ? "正在找歌词…" : "没有找到歌词";
      pane.append(note);
      return;
    }
    const synced = words.synced;
    if (!synced) box.classList.add("plain");
    for (const line of words.lines) {
      const b = document.createElement("button");
      const text = (line.text || "").trim();
      b.textContent = text || " ";
      if (!text) b.classList.add("gap");
      if (synced && line.time != null) {
        b.dataset.time = line.time;
        b.title = `从 ${clock(line.time)} 开始播放`;
        b.onclick = () => seek(line.time);
      }
      box.append(b);
    }
    const buttons = [...box.children];
    rolled.centres = buttons.map((b) => b.offsetTop + b.offsetHeight / 2);
    if (synced) rolled.times = buttons.map((b) => Number(b.dataset.time ?? NaN));
    markChorusLines();
    if (synced) roll(lastPosition);
  }

  function setOffset(offset, animate) {
    const box = $("lyrics-lines");
    const [low, high] = offsetRange();
    rolled.offset = Math.min(high, Math.max(low, offset));
    box.style.transition = animate ? "" : "none";
    box.style.transform = `translateY(${rolled.offset}px)`;
  }

  /** Synced: first and last lines can each reach the middle. Plain: the
   *  block scrolls between its top and bottom edges. */
  function offsetRange() {
    const { centres, times } = rolled;
    const h = $("lyrics").clientHeight;
    if (!centres.length) return [0, 0];
    if (times.length) return [h / 2 - centres[centres.length - 1], h / 2 - centres[0]];
    return [Math.min(0, h - $("lyrics-lines").offsetHeight - 12), 0];
  }

  /** Centre the line being sung at `position`. Index 0 is a real line. */
  function roll(position) {
    const { times, centres } = rolled;
    if (!times.length) return;
    let active = -1;
    for (let i = 0; i < times.length; i++) {
      if (!Number.isNaN(times[i]) && times[i] <= position + 0.15) active = i;
    }
    const box = $("lyrics-lines");
    if (active !== rolled.active) {
      box.children[rolled.active]?.classList.remove("active");
      rolled.active = active;
      if (active >= 0) box.children[active]?.classList.add("active");
    }
    if (Date.now() < manualUntil) return;
    const at = active < 0 ? 0 : active;
    const target = $("lyrics").clientHeight / 2 - centres[at];
    if (Math.abs(target - rolled.offset) > 0.5) setOffset(target, true);
  }

  // Wheel scrolls the lyrics; the roll takes over again after MANUAL_MS.
  $("lyrics").addEventListener(
    "wheel",
    (e) => {
      if (!rolled.centres.length) return;
      e.preventDefault();
      manualUntil = Date.now() + MANUAL_MS;
      setOffset(rolled.offset - e.deltaY, false);
    },
    { passive: false },
  );

  function renderCover() {
    const card = view?.card;
    const url = card && covers[card.id];
    $("cover-img").hidden = !url;
    if (url) $("cover-img").src = url;
    else $("cover-img").removeAttribute("src");
  }

  async function render(next) {
    const changed = view?.card?.id !== next.card?.id;
    await leaving;
    view = next;
    renderStatus();
    if (view.scanning) {
      showEmpty("⏳", "正在读取曲库…", view.scanning, { label: "选择其他文件夹…", run: chooseFolder });
      return;
    }
    if (!view.folder) {
      showEmpty("🎵", "选择一个音乐文件夹", "会依次播放还没评过的每首歌的副歌。", {
        label: "选择文件夹…",
        primary: true,
        run: chooseFolder,
      });
      return;
    }
    if (!view.card) {
      showEmpty(
        "🎉",
        "全部筛完了",
        view.disliked
          ? `一共 ${view.total} 首：喜欢 ${view.liked}，不喜欢 ${view.disliked}。`
          : `一共 ${view.total} 首，没有不喜欢的。`,
        view.disliked
          ? { label: `清理不喜欢的 (${view.disliked})…`, primary: true, run: openSheet }
          : { label: "换个文件夹…", run: chooseFolder },
      );
      return;
    }
    $("empty").hidden = true;
    $("card").hidden = false;
    if (changed) {
      const cover = $("cover");
      cover.className = "cover arrive";
      $("title").textContent = view.card.title;
      $("subtitle").textContent = [view.card.artist, view.card.album].filter(Boolean).join(" — ");
      $("subtitle").title = view.card.file;
      $("namesakes").hidden = !view.card.sameName;
      $("namesakes").textContent = view.card.sameName ? `后面还有 ${view.card.sameName} 首同名歌` : "";
      $("dur").textContent = clock(view.card.duration);
      $("pos").textContent = "0:00";
      $("head").style.transition = "none";
      $("head").style.left = "0";
      $("played").style.width = "0";
      lastPosition = 0;
      setPlaying(false);
      renderCover();
      renderLyrics();
      fetchExtras(view.card.id);
      requestAnimationFrame(() => requestAnimationFrame(() => (cover.className = "cover")));
    }
    renderChorus();
    busy = false;
  }

  /** Catch up on a cover or lyrics sent before this page was listening. */
  async function fetchExtras(id) {
    if (covers[id] !== undefined && lyricsById[id] !== undefined) return;
    const got = await invoke("sorter_extras", { id });
    if ("cover" in got && covers[id] === undefined) covers[id] = got.cover;
    if ("lyrics" in got && lyricsById[id] === undefined) lyricsById[id] = got.lyrics;
    if (view?.card?.id === id) {
      renderCover();
      renderLyrics();
    }
  }

  function setPlaying(on) {
    $("play-icon").innerHTML = on
      ? '<rect x="6" y="5" width="4" height="14" rx="1"/><rect x="14" y="5" width="4" height="14" rx="1"/>'
      : '<path d="M8 5v14l11-7z"/>';
  }

  // --------------------------------------------------------------- actions

  async function chooseFolder() {
    try {
      await invoke("sorter_choose_folder");
    } catch (err) {
      toast(String(err));
    }
  }

  function decide(like) {
    if (busy || !view?.card) return;
    busy = true;
    const cover = $("cover");
    const button = $(like ? "like" : "dislike");
    button.classList.add("pressed");
    setTimeout(() => button.classList.remove("pressed"), 120);
    cover.className = `cover ${like ? "go-like" : "go-dislike"}`;
    leaving = new Promise((done) => setTimeout(done, 230));
    invoke("sorter_rate", { like }).catch((err) => {
      busy = false;
      cover.className = "cover";
      toast(String(err));
    });
  }

  function seek(at) {
    if (!view?.card) return;
    lastPosition = at;
    showPosition(at, false);
    invoke("sorter_seek", { at }).catch((err) => toast(String(err)));
  }

  function showPosition(position, animate) {
    const d = view?.card?.duration || 1;
    const pct = `${Math.min(100, Math.max(0, (position / d) * 100))}%`;
    $("head").style.transition = animate ? "" : "none";
    $("played").style.transition = animate ? "" : "none";
    $("head").style.left = pct;
    $("played").style.width = pct;
    $("pos").textContent = clock(position);
  }

  function skip() {
    if (busy || !view?.card || view.remaining < 2) return;
    busy = true;
    invoke("sorter_skip").finally(() => (busy = false));
  }

  function undo() {
    if (busy || !view?.canUndo) return;
    invoke("sorter_undo");
  }

  // ----------------------------------------------------------------- sheet

  async function openSheet() {
    const rows = await invoke("sorter_disliked");
    const list = $("sheet-list");
    list.replaceChildren(
      ...rows.map((row) => {
        const li = document.createElement("li");
        const box = document.createElement("input");
        box.type = "checkbox";
        box.checked = true;
        box.value = row.id;
        box.onchange = syncSheet;
        const text = document.createElement("div");
        const b = document.createElement("b");
        b.textContent = row.title;
        const span = document.createElement("span");
        span.textContent = [row.artist, row.album].filter(Boolean).join(" — ");
        span.title = row.file;
        text.append(b, span);
        li.append(box, text);
        li.onclick = (e) => {
          if (e.target !== box) {
            box.checked = !box.checked;
            syncSheet();
          }
        };
        return li;
      }),
    );
    $("check-all").checked = true;
    syncSheet();
    $("sheet").hidden = false;
  }

  function checked() {
    return [...$("sheet-list").querySelectorAll("input:checked")].map((box) => box.value);
  }

  function syncSheet() {
    const n = checked().length;
    const all = $("sheet-list").querySelectorAll("input").length;
    $("check-all").checked = n === all && all > 0;
    $("check-all").indeterminate = n > 0 && n < all;
    $("sheet-trash").disabled = n === 0;
    $("sheet-trash").textContent = n ? `移到废纸篓 (${n})` : "移到废纸篓";
  }

  function closeSheet() {
    $("sheet").hidden = true;
  }

  $("check-all").onchange = () => {
    const on = $("check-all").checked;
    $("sheet-list").querySelectorAll("input").forEach((box) => (box.checked = on));
    syncSheet();
  };
  $("sheet-cancel").onclick = closeSheet;
  $("sheet-trash").onclick = async () => {
    try {
      const moved = await invoke("sorter_trash", { ids: checked() });
      if (moved) {
        closeSheet();
        toast(`已把 ${moved} 首移到废纸篓`);
      }
    } catch (err) {
      toast(String(err));
    }
  };

  // ------------------------------------------------------------------ input

  $("folder").onclick = chooseFolder;
  $("review").onclick = openSheet;
  $("reset").onclick = () => invoke("sorter_reset_ratings");
  $("like").onclick = () => decide(true);
  $("dislike").onclick = () => decide(false);
  $("skip").onclick = skip;
  $("undo").onclick = undo;
  $("play").onclick = () => invoke("sorter_toggle_play");
  $("replay").onclick = () => invoke("sorter_replay");

  // Drag (or click) anywhere on the bar to play from there.
  const track = $("track");
  const at = (e) => {
    const r = track.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
    return f * (view?.card?.duration || 0);
  };
  track.addEventListener("mousedown", (e) => {
    if (e.button !== 0 || !view?.card) return;
    e.preventDefault();
    dragging = true;
    $("timeline").classList.add("dragging");
    showPosition(at(e), false);
  });
  document.addEventListener("mousemove", (e) => {
    if (dragging) showPosition(at(e), false);
  });
  document.addEventListener("mouseup", (e) => {
    if (!dragging) return;
    dragging = false;
    $("timeline").classList.remove("dragging");
    seek(at(e));
  });

  // Left click on the cover likes, right click dislikes.
  const cover = $("cover");
  cover.addEventListener("click", (e) => {
    if (e.button === 0) decide(true);
  });
  cover.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    decide(false);
  });
  cover.addEventListener("mousemove", (e) => {
    if (busy) return;
    const r = cover.getBoundingClientRect();
    const left = e.clientX < r.left + r.width / 2;
    cover.classList.toggle("lean-like", left);
    cover.classList.toggle("lean-dislike", !left);
  });
  cover.addEventListener("mouseleave", () => cover.classList.remove("lean-like", "lean-dislike"));

  // WebKit's own context menu (Reload, Inspect) is not part of this app.
  document.addEventListener("contextmenu", (e) => e.preventDefault());

  document.addEventListener("keydown", (e) => {
    if (!$("sheet").hidden) {
      if (e.key === "Escape") closeSheet();
      return;
    }
    if (e.metaKey && e.key.toLowerCase() === "z") {
      e.preventDefault();
      undo();
      return;
    }
    if (e.metaKey || e.ctrlKey || e.altKey || e.repeat) return;
    const keys = {
      ArrowLeft: () => decide(true),
      ArrowRight: () => decide(false),
      ArrowDown: skip,
      " ": () => invoke("sorter_toggle_play"),
      r: () => invoke("sorter_replay"),
      R: () => invoke("sorter_replay"),
    };
    const run = keys[e.key];
    if (run && view?.card) {
      e.preventDefault();
      run();
    }
  });

  // ----------------------------------------------------------------- events

  listen("sorter-view", (e) => render(e.payload));
  listen("sorter-cover", (e) => {
    covers[e.payload.id] = e.payload.url;
    if (view?.card?.id === e.payload.id) renderCover();
  });
  listen("sorter-chorus", (e) => {
    choruses[e.payload.id] = e.payload.chorus;
    if (view?.card?.id === e.payload.id) renderChorus();
  });
  listen("sorter-lyrics", (e) => {
    lyricsById[e.payload.id] = e.payload.lyrics;
    if (view?.card?.id === e.payload.id) renderLyrics();
  });
  listen("sorter-progress", (e) => {
    const p = e.payload;
    if (view?.card?.id !== p.id) return;
    setPlaying(p.playing);
    if (dragging) return;
    // A jump backwards (a loop restarting) should not sweep across the bar.
    showPosition(p.position, p.playing && p.position >= lastPosition);
    lastPosition = p.position;
    roll(p.position);
  });
  listen("sorter-error", (e) => toast(e.payload));

  invoke("sorter_view").then((v) => v && render(v));
})();
