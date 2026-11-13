<script lang="ts">
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { revealItemInDir } from "@tauri-apps/plugin-opener";
  import { onDestroy, onMount } from "svelte";
  import type { PosterInfo, Provider, Settings, SourceInfo, Style } from "$lib/types";

  interface Props {
    settings: Settings;
    t: (k: string) => string;
    live: boolean;
    apiKeys: Record<string, boolean>;
    videoSources: SourceInfo[];
    segments: { text: string; final: boolean }[];
    sourceLabel: (id: string) => string;
    onaddtoscene: (path: string) => void;
    onerror: (msg: string) => void;
    onkeys: () => void;
  }
  let { settings, t, live, apiKeys, segments, sourceLabel, onaddtoscene, onerror, onkeys }: Props = $props();

  const p = $derived(settings.posters);
  const PREVIEW_BASE = "https://raw.githubusercontent.com/Xalcker/image-gen-styles/main/catalog/previews/";
  const DEFAULT_MODELS: Record<Provider, string> = { gemini: "gemini-3.1-flash-image", openai: "gpt-image-2.6-flare" };

  let styles = $state<Style[]>([]);
  let categories = $state<{ id: string; label: string }[]>([]);
  let category = $state("");
  let stylesError = $state(false);
  let gallery = $state<PosterInfo[]>([]);
  let frame = $state<string | null>(null);
  let generating = $state(false);
  let keyInput = $state("");
  let models = $state<string[]>([]);

  // Fuentes que existen en alguna escena (solo esas se pueden capturar).
  const sources = $derived([
    "program",
    ...new Set(
      settings.scenes
        .flatMap((s) => s.layers)
        .map((l) => l.source)
        .filter((src) => !src.startsWith("text:") && !src.startsWith("color:")),
    ),
  ]);
  const style = $derived(styles.find((s) => s.id === p.styleId) ?? null);
  const shown = $derived(styles.filter((s) => !category || s.category === category));
  const aspect = $derived(p.aspect === "1:1" ? "1:1" : settings.orientation === "portrait" ? "9:16" : "16:9");

  onMount(async () => {
    if (!p.template) p.template = t("poster_default_template");
    try {
      const v = await invoke<{ styles: Style[]; categories: { id: string; label: string }[] }>("poster_styles");
      styles = v.styles;
      categories = v.categories;
    } catch {
      stylesError = true;
    }
    await refreshGallery();
  });

  async function refreshGallery() {
    gallery = await invoke<PosterInfo[]>("list_posters").catch(() => []);
  }

  /** Resuelve las variables de la plantilla. */
  function renderTemplate(tpl: string) {
    const recent = segments
      .filter((s) => s.final && s.text.trim())
      .slice(-6)
      .map((s) => s.text.trim())
      .join(" ");
    const now = new Date();
    const vars: Record<string, string> = {
      titulo: p.title.trim(),
      title: p.title.trim(),
      transcripcion: recent || "(sin transcripción)",
      transcript: recent || "(no transcript)",
      fecha: now.toLocaleDateString(),
      date: now.toLocaleDateString(),
      hora: now.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
      time: now.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
      aspecto: aspect,
    };
    return tpl.replace(/\{(\w+)\}/g, (m, k) => vars[k] ?? m);
  }

  const finalPrompt = $derived([style?.prompt, renderTemplate(p.template)].filter(Boolean).join("\n\n"));

  async function showFrame() {
    try {
      frame = "data:image/jpeg;base64," + (await invoke<string>("capture_frame", { source: p.source }));
    } catch (e) {
      onerror(String(e) === "not-running" ? t("not_running") : String(e));
    }
  }

  async function generate() {
    if (generating) return;
    generating = true;
    try {
      const info = await invoke<PosterInfo>("generate_poster", {
        request: {
          provider: p.provider,
          model: p.model,
          prompt: finalPrompt,
          aspect,
          quality: p.quality,
          source: p.source,
          styleId: p.styleId,
        },
      });
      gallery = [info, ...gallery];
    } catch (e) {
      const m = String(e);
      onerror(m === "no-api-key" ? t("tr_no_key") : m === "not-running" ? t("not_running") : m);
    } finally {
      generating = false;
    }
  }

  // Generación automática cada N minutos.
  let timer: ReturnType<typeof setInterval> | undefined;
  $effect(() => {
    clearInterval(timer);
    const mins = p.autoMinutes;
    if (mins > 0) timer = setInterval(generate, mins * 60_000);
    return () => clearInterval(timer);
  });
  onDestroy(() => clearInterval(timer));

  async function saveKey() {
    await invoke("set_api_key", { provider: p.provider, key: keyInput }).catch((e) => onerror(String(e)));
    keyInput = "";
    onkeys();
  }

  async function loadModels() {
    try {
      models = (await invoke<string[]>("list_models", { provider: p.provider, purpose: "text" })).filter((m) => m.includes("image"));
    } catch (e) {
      onerror(String(e) === "no-api-key" ? t("tr_no_key") : String(e));
    }
  }

  async function remove(path: string) {
    await invoke("delete_poster", { path }).catch((e) => onerror(String(e)));
    await refreshGallery();
  }

  function insertVar(v: string) {
    p.template = `${p.template}{${v}}`;
  }
</script>

<fieldset>
  <h2>{t("posters")}</h2>

  <label>
    {t("poster_source")}
    <div class="inline">
      <select bind:value={p.source}>
        {#each sources as s}<option value={s}>{sourceLabel(s)}</option>{/each}
      </select>
      <button class="ghost small" onclick={showFrame}>{t("preview_frame")}</button>
    </div>
  </label>
  {#if frame}<img class="frame" src={frame} alt="frame" />{/if}

  <div class="label">{t("style")}</div>
  {#if stylesError}
    <p class="hint err">{t("styles_offline")}</p>
  {:else}
    <div class="cats">
      <button class:on={!category} onclick={() => (category = "")}>{t("all")}</button>
      {#each categories as c}<button class:on={category === c.id} onclick={() => (category = c.id)}>{c.label}</button>{/each}
    </div>
    <div class="styles">
      <button class="style none" class:sel={!p.styleId} onclick={() => (p.styleId = null)}><span>{t("no_style")}</span></button>
      {#each shown as s (s.id)}
        <button class="style" class:sel={p.styleId === s.id} title={s.description} onclick={() => (p.styleId = s.id)}>
          <img src={PREVIEW_BASE + s.id + ".jpg"} alt="" loading="lazy" onerror={(e) => ((e.currentTarget as HTMLImageElement).style.visibility = "hidden")} />
          <span>{s.name}</span>
        </button>
      {/each}
    </div>
  {/if}

  <label>{t("poster_title")}<input type="text" bind:value={p.title} /></label>
  <label>
    {t("template")}
    <textarea rows="4" bind:value={p.template}></textarea>
  </label>
  <div class="vars">
    <span class="hint">{t("variables")}:</span>
    {#each ["titulo", "transcripcion", "fecha", "hora"] as v}<button class="chipbtn" onclick={() => insertVar(v)}>{`{${v}}`}</button>{/each}
  </div>
  <details>
    <summary class="hint">Prompt</summary>
    <pre>{finalPrompt}</pre>
  </details>

  <div class="row">
    <span>{t("aspect")}</span>
    <div class="seg">
      <button class:on={p.aspect === "canvas"} onclick={() => (p.aspect = "canvas")}>{t("aspect_canvas")} ({settings.orientation === "portrait" ? "9:16" : "16:9"})</button>
      <button class:on={p.aspect === "1:1"} onclick={() => (p.aspect = "1:1")}>1:1</button>
    </div>
  </div>
  <div class="row">
    <span>{t("quality")}</span>
    <div class="seg">
      {#each ["low", "medium", "high"] as q}<button class:on={p.quality === q} onclick={() => (p.quality = q as typeof p.quality)}>{t("q_" + q)}</button>{/each}
    </div>
  </div>
  <div class="row">
    <span>{t("provider")}</span>
    <div class="seg">
      <button class:on={p.provider === "gemini"} onclick={() => (p.provider = "gemini")}>Gemini</button>
      <button class:on={p.provider === "openai"} onclick={() => (p.provider = "openai")}>OpenAI</button>
    </div>
  </div>
  <label>
    {t("api_key")} <small class:ok={apiKeys[p.provider]}>({apiKeys[p.provider] ? t("key_saved") : t("key_missing")})</small>
    <div class="inline">
      <input type="password" autocomplete="off" bind:value={keyInput} placeholder="••••••••" />
      <button class="ghost small" onclick={saveKey}>{t("save")}</button>
    </div>
  </label>
  <label>
    {t("model")}
    <div class="inline">
      <input type="text" spellcheck="false" list="img-models" bind:value={p.model} placeholder={DEFAULT_MODELS[p.provider]} />
      <button class="ghost small" title={t("load_models")} onclick={loadModels}>↻</button>
    </div>
    <datalist id="img-models">{#each models as m}<option value={m}></option>{/each}</datalist>
  </label>
  <label class="row">
    <span>{t("auto_every")}</span>
    <input type="number" min="0" max="120" class="mins" bind:value={p.autoMinutes} />
  </label>

  <button class="primary" disabled={generating} onclick={generate}>{generating ? t("generating") : t("generate")}</button>
  <p class="hint">{t("poster_privacy")}</p>

  {#if gallery.length}
    <div class="label">{t("gallery")}</div>
    <div class="gallery">
      {#each gallery as g (g.path)}
        <figure>
          <img src={convertFileSrc(g.path)} alt="poster" loading="lazy" title={g.prompt} />
          <figcaption>
            <button class="ghost small" onclick={() => onaddtoscene(g.path)}>＋ {t("add_to_scene")}</button>
            <button class="icon" title={t("show_in_folder")} onclick={() => revealItemInDir(g.path)}>📁</button>
            <button class="icon" title={t("delete")} onclick={() => remove(g.path)}>✕</button>
          </figcaption>
        </figure>
      {/each}
    </div>
  {/if}
  {#if live}<p class="hint">{t("live_poster_hint")}</p>{/if}
</fieldset>

<style>
  fieldset { border: 0; border-bottom: 1px solid var(--line); margin: 0; padding: 12px 16px; display: flex; flex-direction: column; gap: 10px; }
  h2 { margin: 0; font-size: 11px; text-transform: uppercase; letter-spacing: 0.8px; color: var(--muted); }
  label, .label { display: flex; flex-direction: column; gap: 4px; color: var(--muted); }
  label.row { flex-direction: row; align-items: center; justify-content: space-between; }
  label small.ok { color: var(--ok); }
  .row { display: flex; align-items: center; justify-content: space-between; color: var(--muted); gap: 6px; }
  .inline { display: flex; gap: 6px; }
  .inline input, .inline select { flex: 1; min-width: 0; }
  select, input, textarea {
    background: var(--bg); color: var(--text); border: 1px solid var(--line); border-radius: 5px; padding: 6px 8px; font: inherit; user-select: text;
  }
  textarea { resize: vertical; }
  .mins { width: 64px; }
  .seg { display: flex; }
  .seg button, .cats button {
    background: transparent; border: 1px solid var(--line); color: var(--muted); padding: 4px 8px; cursor: pointer; font: inherit; font-size: 12px;
  }
  .seg button:first-child { border-radius: 5px 0 0 5px; }
  .seg button:last-child { border-radius: 0 5px 5px 0; }
  .seg button.on, .cats button.on { background: var(--accent); border-color: var(--accent); color: #fff; }
  .cats { display: flex; flex-wrap: wrap; gap: 4px; }
  .cats button { border-radius: 999px; }
  .styles { display: grid; grid-template-columns: repeat(3, 1fr); gap: 6px; max-height: 260px; overflow-y: auto; }
  .style {
    position: relative; aspect-ratio: 1; padding: 0; border: 2px solid transparent; border-radius: 6px; overflow: hidden;
    background: var(--bg); cursor: pointer;
  }
  .style.sel { border-color: var(--accent); }
  .style img { width: 100%; height: 100%; object-fit: cover; display: block; }
  .style span {
    position: absolute; left: 0; right: 0; bottom: 0; padding: 2px 4px; font-size: 10px; color: #fff;
    background: linear-gradient(transparent, rgba(0, 0, 0, 0.8)); text-align: left;
  }
  .style.none { display: flex; align-items: center; justify-content: center; }
  .style.none span { position: static; background: none; color: var(--muted); text-align: center; }
  .vars { display: flex; flex-wrap: wrap; gap: 4px; align-items: center; }
  .chipbtn { background: var(--line); border: 0; color: var(--text); border-radius: 4px; padding: 2px 6px; font-size: 11px; cursor: pointer; }
  pre { white-space: pre-wrap; font-size: 11px; color: var(--muted); margin: 4px 0 0; user-select: text; }
  .frame { width: 100%; border-radius: 6px; }
  .primary { background: var(--accent); color: #fff; border: 0; border-radius: 6px; padding: 10px; font: inherit; font-weight: 700; cursor: pointer; }
  .primary:disabled { opacity: 0.6; cursor: progress; }
  .gallery { display: grid; grid-template-columns: repeat(2, 1fr); gap: 8px; }
  figure { margin: 0; display: flex; flex-direction: column; gap: 4px; }
  figure img { width: 100%; border-radius: 6px; background: #000; }
  figcaption { display: flex; align-items: center; gap: 2px; }
  .ghost { background: transparent; border: 1px solid var(--line); color: var(--text); border-radius: 5px; padding: 4px 8px; cursor: pointer; font: inherit; font-size: 12px; }
  .icon { background: transparent; border: 0; cursor: pointer; color: var(--muted); }
  .hint { margin: 0; color: var(--muted); font-size: 12px; }
  .hint.err { color: #ffb3bb; }
  details summary { cursor: pointer; }
</style>
