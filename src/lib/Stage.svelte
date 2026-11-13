<script lang="ts">
  import type { Layer, Rect } from "$lib/types";

  interface Props {
    frame: string | null;
    canvas: { w: number; h: number };
    layers: Layer[];
    selectedId: string | null;
    labelOf: (l: Layer) => string;
    onselect: (id: string | null) => void;
    /** Se llama mientras se arrastra (cambios en vivo). */
    onrect: (id: string, rect: Rect) => void;
    emptyText: string;
  }
  let { frame, canvas, layers, selectedId, labelOf, onselect, onrect, emptyText }: Props = $props();

  let wrap: HTMLDivElement;
  let box = $state({ w: 0, h: 0 });

  // Ajusta el lienzo al espacio disponible manteniendo su aspecto.
  $effect(() => {
    const ro = new ResizeObserver(() => {
      const r = wrap.getBoundingClientRect();
      const scale = Math.min(r.width / canvas.w, r.height / canvas.h);
      box = { w: Math.floor(canvas.w * scale), h: Math.floor(canvas.h * scale) };
    });
    ro.observe(wrap);
    return () => ro.disconnect();
  });

  type Handle = "move" | "nw" | "ne" | "sw" | "se";
  let drag: { id: string; handle: Handle; start: Rect; px: number; py: number; aspect: number } | null = null;

  const SNAP = 0.015;
  const MIN = 0.03;

  function snap(v: number, targets: number[]) {
    for (const t of targets) if (Math.abs(v - t) < SNAP) return t;
    return v;
  }

  function down(e: PointerEvent, layer: Layer, handle: Handle) {
    e.stopPropagation();
    e.preventDefault();
    onselect(layer.id);
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    drag = {
      id: layer.id,
      handle,
      start: { ...layer.rect },
      px: e.clientX,
      py: e.clientY,
      aspect: layer.rect.w / layer.rect.h,
    };
  }

  function move(e: PointerEvent) {
    if (!drag || box.w === 0) return;
    const dx = (e.clientX - drag.px) / box.w;
    const dy = (e.clientY - drag.py) / box.h;
    const s = drag.start;
    let r: Rect = { ...s };
    if (drag.handle === "move") {
      r.x = s.x + dx;
      r.y = s.y + dy;
      // Imantar bordes y centro al lienzo.
      r.x = snap(r.x, [0, 0.5 - r.w / 2, 1 - r.w]);
      r.y = snap(r.y, [0, 0.5 - r.h / 2, 1 - r.h]);
    } else {
      let left = s.x, top = s.y, right = s.x + s.w, bottom = s.y + s.h;
      if (drag.handle.includes("w")) left = snap(Math.min(s.x + dx, right - MIN), [0, 0.5]);
      if (drag.handle.includes("e")) right = snap(Math.max(s.x + s.w + dx, left + MIN), [1, 0.5]);
      if (drag.handle.includes("n")) top = snap(Math.min(s.y + dy, bottom - MIN), [0, 0.5]);
      if (drag.handle.includes("s")) bottom = snap(Math.max(s.y + s.h + dy, top + MIN), [1, 0.5]);
      r = { x: left, y: top, w: right - left, h: bottom - top };
      // Shift: conservar proporción.
      if (e.shiftKey) {
        const h = r.w / drag.aspect;
        r.h = h;
        if (drag.handle.includes("n")) r.y = bottom - h;
      }
    }
    onrect(drag.id, r);
  }

  function up() {
    drag = null;
  }

  // Capa superior primero para que el clic alcance la de arriba.
  const ordered = $derived([...layers].reverse());
</script>

<div class="wrap" bind:this={wrap}>
  <div class="canvas" role="application" style="width:{box.w}px;height:{box.h}px" onpointerdown={() => onselect(null)}>
    {#if frame}
      <img src={frame} alt="preview" draggable="false" />
    {:else}
      <div class="empty">{emptyText}</div>
    {/if}
    <div class="overlay" role="presentation" onpointermove={move} onpointerup={up} onpointercancel={up}>
      {#each ordered as layer (layer.id)}
        {@const r = layer.rect}
        <div
          class="layer"
          class:selected={layer.id === selectedId}
          class:hidden={!layer.visible}
          style="left:{r.x * 100}%;top:{r.y * 100}%;width:{r.w * 100}%;height:{r.h * 100}%"
          onpointerdown={(e) => down(e, layer, "move")}
          role="button"
          tabindex="-1"
        >
          {#if layer.id === selectedId}
            <span class="tag">{labelOf(layer)}</span>
            {#each ["nw", "ne", "sw", "se"] as const as hd}
              <span class="handle {hd}" onpointerdown={(e) => down(e, layer, hd)} role="presentation"></span>
            {/each}
          {/if}
        </div>
      {/each}
    </div>
  </div>
</div>

<style>
  .wrap {
    flex: 1;
    min-height: 0;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .canvas {
    position: relative;
    background: #000;
    border-radius: 6px;
    overflow: hidden;
  }
  img {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    pointer-events: none;
  }
  .empty {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--muted);
  }
  .overlay {
    position: absolute;
    inset: 0;
  }
  .layer {
    position: absolute;
    box-sizing: border-box;
    cursor: move;
    outline: 1px dashed transparent;
  }
  .layer:hover {
    outline-color: rgba(255, 255, 255, 0.35);
  }
  .layer.hidden {
    pointer-events: none;
  }
  .layer.selected {
    outline: 2px solid var(--accent);
  }
  .tag {
    position: absolute;
    left: 0;
    top: 0;
    transform: translateY(-100%);
    background: var(--accent);
    color: #fff;
    font-size: 11px;
    padding: 1px 6px;
    white-space: nowrap;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .layer.selected .tag {
    transform: none;
  }
  .handle {
    position: absolute;
    width: 12px;
    height: 12px;
    background: #fff;
    border: 2px solid var(--accent);
    border-radius: 2px;
  }
  .nw { left: -6px; top: -6px; cursor: nwse-resize; }
  .ne { right: -6px; top: -6px; cursor: nesw-resize; }
  .sw { left: -6px; bottom: -6px; cursor: nesw-resize; }
  .se { right: -6px; bottom: -6px; cursor: nwse-resize; }
</style>
