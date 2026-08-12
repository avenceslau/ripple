use serde::Serialize;

#[derive(Serialize)]
pub struct GraphView {
    pub package: Option<String>,
    pub base: String,
    pub task: String,
    pub scope: String,
    pub nodes: Vec<GraphNode>,
    pub links: Vec<GraphLink>,
}

#[derive(Serialize)]
pub struct GraphNode {
    pub id: usize,
    pub label: String,
    pub file: String,
    pub symbol: String,
    pub package: String,
    pub kind: NodeKind,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<Vec<usize>>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Seed,
    Affected,
    Dependency,
    Target,
    Normal,
}

#[derive(Serialize)]
pub struct GraphLink {
    pub source: usize,
    pub target: usize,
    #[serde(rename = "type")]
    pub type_only: bool,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

pub fn render_html(view: &GraphView) -> String {
    let data = serde_json::to_string(view).unwrap_or_else(|_| "{}".to_string());
    TEMPLATE.replace("/*__DATA__*/null", &data)
}

const TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>monoripple graph</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; font-family: ui-sans-serif, system-ui, sans-serif; background: #0f1117; color: #e6e6e6; }
  button, input { font: inherit; }
  #app { display: grid; grid-template-columns: minmax(0, 1fr) 380px; grid-template-rows: auto 1fr; height: 100vh; }
  header { grid-column: 1 / 3; padding: 10px 16px; border-bottom: 1px solid #232732; display: flex; gap: 14px; align-items: center; flex-wrap: wrap; }
  header h1 { font-size: 15px; margin: 0; font-weight: 650; }
  header .meta { color: #8b93a7; font-size: 12px; }
  header .flow { color: #cbd5e1; font-size: 12px; padding: 4px 8px; background: #171a22; border-radius: 999px; }
  header input { margin-left: auto; background: #171a22; border: 1px solid #2a2f3b; color: #e6e6e6; padding: 6px 10px; border-radius: 6px; width: 220px; }
  header button { background: #171a22; border: 1px solid #2a2f3b; color: #cbd5e1; padding: 6px 9px; border-radius: 6px; cursor: pointer; }
  header button:hover { border-color: #475569; color: #fff; }
  #graph { position: relative; min-width: 0; overflow: hidden; }
  canvas { display: block; width: 100%; height: 100%; }
  #empty { display: none; position: absolute; inset: 0; place-items: center; color: #8b93a7; }
  #side { border-left: 1px solid #232732; padding: 16px 18px; overflow: auto; background: #11141b; }
  #side h2 { font-size: 11px; margin: 18px 0 7px; color: #8b93a7; text-transform: uppercase; letter-spacing: .08em; }
  #side h2:first-child { margin-top: 0; }
  #side .node-title { font-size: 15px; font-weight: 650; line-height: 1.35; word-break: break-word; }
  #side .value { font-size: 13px; line-height: 1.45; word-break: break-word; color: #d7dce7; }
  #side .meta-grid { display: grid; grid-template-columns: 58px 1fr; gap: 5px 9px; margin-top: 10px; font-size: 12px; }
  #side .meta-grid dt { color: #727b90; }
  #side .meta-grid dd { margin: 0; word-break: break-word; }
  #side ul { margin: 6px 0 12px; padding-left: 18px; }
  #side li { font-size: 12px; margin-bottom: 5px; line-height: 1.4; }
  .legend { display: flex; gap: 12px; font-size: 11px; color: #aab2c3; flex-wrap: wrap; }
  .dot { display: inline-block; width: 9px; height: 9px; border-radius: 50%; margin-right: 4px; vertical-align: middle; }
  .edge-sample { display: inline-block; width: 18px; border-top: 2px solid #94a3b8; margin-right: 5px; vertical-align: middle; }
  .edge-sample.type { border-top-style: dashed; border-color: #60a5fa; }
  .hint { color: #687084; font-size: 11px; line-height: 1.45; margin-top: 18px; }
  .path-options { display: flex; gap: 5px; flex-wrap: wrap; margin: 7px 0 10px; }
  .path-options button { border: 1px solid #303746; border-radius: 5px; background: #171a22; color: #9aa4b8; padding: 4px 7px; cursor: pointer; font-size: 11px; }
  .path-options button.active { border-color: #a78bfa; background: #211d35; color: #ddd6fe; }
  .path { list-style: none; margin: 8px 0 14px; padding: 0; }
  .path li { position: relative; margin: 0 0 8px; padding-left: 34px; }
  .step-number { position: absolute; left: 0; top: 1px; display: grid; place-items: center; width: 25px; height: 25px; border: 1px solid #475569; border-radius: 50%; background: #171a22; color: #cbd5e1; font-size: 11px; font-weight: 700; }
  .path-reason { margin: 4px 0 7px -21px; padding: 5px 7px 5px 18px; border-left: 1px solid #64748b; color: #aeb7c8; font-size: 11px; line-height: 1.4; }
  .path-reason small { display: block; color: #707a90; font-family: ui-monospace, monospace; font-size: 10px; }
  .path button, .relation button, .targets button { width: 100%; padding: 5px 7px; border: 0; border-radius: 5px; background: transparent; color: #dce3ee; text-align: left; cursor: pointer; }
  .path button:hover, .relation button:hover, .targets button:hover { background: #1b202b; }
  .path strong { display: block; font-size: 12px; word-break: break-word; }
  .path button small { display: block; margin-top: 2px; color: #7f899e; font-family: ui-monospace, monospace; font-size: 10px; word-break: break-word; }
  .relation, .targets { list-style: none; padding: 0 !important; }
  .relation button small { display: block; margin-top: 3px; color: #737e94; font-size: 10px; line-height: 1.35; }
  .relation-title { display: flex; justify-content: space-between; gap: 8px; }
  .relation .edge-kind { color: #6f7a91; font-size: 10px; white-space: nowrap; }
  .targets button { color: #c4b5fd; }
  .targets button.active { background: #211d35; color: #ddd6fe; }
  .kind { display: inline-block; padding: 2px 6px; border-radius: 999px; background: #252a36; font-size: 10px; color: #cbd5e1; }
  @media (max-width: 820px) {
    #app { grid-template-columns: 1fr; grid-template-rows: auto minmax(380px, 58vh) auto; height: auto; min-height: 100vh; }
    header { grid-column: 1; }
    header input { margin-left: 0; }
    #side { border-left: 0; border-top: 1px solid #232732; }
  }
</style>
</head>
<body>
<div id="app">
  <header>
    <h1>monoripple graph</h1>
    <span class="meta" id="summary"></span>
    <span class="flow">changed → consumers → targets</span>
    <span class="legend">
      <span><span class="dot" style="background:#f97316"></span>changed</span>
      <span><span class="dot" style="background:#38bdf8"></span>affected</span>
      <span><span class="dot" style="background:#a78bfa"></span>target</span>
      <span><span class="edge-sample"></span>runtime</span>
      <span><span class="edge-sample type"></span>type</span>
    </span>
    <input id="filter" placeholder="find a file, symbol, or package…" />
    <button id="fit" type="button">Fit graph</button>
    <button id="clear" type="button">Show all</button>
  </header>
  <main id="graph">
    <canvas id="canvas"></canvas>
    <div id="empty">No graph nodes to display.</div>
  </main>
  <aside id="side"></aside>
</div>
<script>
const DATA = /*__DATA__*/null;
const canvas = document.getElementById("canvas");
const side = document.getElementById("side");
const ctx = canvas.getContext("2d");
const colors = {
  seed: "#f97316",
  affected: "#38bdf8",
  dependency: "#34d399",
  target: "#a78bfa",
  normal: "#4b5568"
};

const nodes = DATA.nodes.map((node) => ({
  ...node,
  x: 0,
  y: 0,
  depth: null
}));
const nodeById = new Map(nodes.map((node) => [node.id, node]));
const links = DATA.links.map((link) => ({
  ...link,
  source: nodeById.get(link.source),
  target: nodeById.get(link.target)
}));
const adjacency = new Map(nodes.map((node) => [node.id, new Set()]));
for (const link of links) {
  adjacency.get(link.source.id).add(link.target.id);
  adjacency.get(link.target.id).add(link.source.id);
}

let width = 0;
let height = 0;
let dpr = window.devicePixelRatio || 1;
let selected = null;
let selectedPathIndex = 0;
let hovered = null;
let dragging = null;
let panning = false;
let moved = false;
let lastX = 0;
let lastY = 0;
let filterText = "";
const view = { x: 0, y: 0, scale: 1 };

function resize() {
  const rect = canvas.getBoundingClientRect();
  width = rect.width;
  height = rect.height;
  dpr = window.devicePixelRatio || 1;
  canvas.width = Math.max(1, Math.round(width * dpr));
  canvas.height = Math.max(1, Math.round(height * dpr));
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}

function assignDepths() {
  for (const node of nodes) {
    node.depth = null;
  }

  if (DATA.scope === "all") {
    const dependencyCounts = new Map(nodes.map((node) => [node.id, 0]));
    const consumers = new Map(nodes.map((node) => [node.id, []]));
    for (const link of links) {
      dependencyCounts.set(link.source.id, dependencyCounts.get(link.source.id) + 1);
      consumers.get(link.target.id).push(link.source);
    }

    const queue = nodes
      .filter((node) => dependencyCounts.get(node.id) === 0)
      .sort((left, right) => left.label.localeCompare(right.label));
    const processed = new Set();
    let queueIndex = 0;
    while (processed.size < nodes.length) {
      if (queueIndex >= queue.length) {
        const cycleRoot = nodes.find((node) => !processed.has(node.id));
        cycleRoot.depth ??= 0;
        dependencyCounts.set(cycleRoot.id, 0);
        queue.push(cycleRoot);
      }

      const dependency = queue[queueIndex++];
      if (processed.has(dependency.id)) {
        continue;
      }
      processed.add(dependency.id);
      dependency.depth ??= 0;

      for (const consumer of consumers.get(dependency.id)) {
        if (processed.has(consumer.id)) {
          continue;
        }
        consumer.depth = Math.max(consumer.depth ?? 0, dependency.depth + 1);
        const remaining = dependencyCounts.get(consumer.id) - 1;
        dependencyCounts.set(consumer.id, remaining);
        if (remaining === 0) {
          queue.push(consumer);
        }
      }
    }
    return;
  }

  for (const node of nodes) {
    for (const path of node.paths || []) {
      for (let index = 0; index < path.length; index++) {
        const step = nodeById.get(path[index]);
        if (step) {
          step.depth = Math.max(step.depth ?? 0, index);
        }
      }
    }
  }

  for (const node of nodes) {
    if (node.kind === "seed" || node.kind === "dependency") {
      node.depth ??= 0;
    }
    if (node.kind === "target") {
      node.depth ??= 1;
    }
  }

  for (let pass = 0; pass < nodes.length; pass++) {
    let changed = false;
    for (const link of links) {
      const dependency = link.target;
      const consumer = link.source;
      if (dependency.depth !== null && consumer.depth === null) {
        consumer.depth = dependency.depth + 1;
        changed = true;
      } else if (consumer.depth !== null && dependency.depth === null) {
        dependency.depth = Math.max(0, consumer.depth - 1);
        changed = true;
      }
    }
    if (!changed) {
      break;
    }
  }

  for (const node of nodes) {
    node.depth ??= 0;
  }
}

function layout() {
  assignDepths();
  const columns = new Map();
  for (const node of nodes) {
    if (!columns.has(node.depth)) {
      columns.set(node.depth, []);
    }
    columns.get(node.depth).push(node);
  }

  const orderedColumns = [...columns.entries()].sort((left, right) => left[0] - right[0]);
  const maxRows = Math.max(1, ...orderedColumns.map(([, column]) => column.length));
  const columnGap = DATA.scope === "all" ? 220 : 270;
  const rowGap = 58;
  const graphHeight = Math.max(height - 100, (maxRows - 1) * rowGap);

  for (const [depth, column] of orderedColumns) {
    column.sort((left, right) => {
      return `${left.package}\u0000${left.file}\u0000${left.symbol}`.localeCompare(
        `${right.package}\u0000${right.file}\u0000${right.symbol}`
      );
    });
    const usedHeight = (column.length - 1) * rowGap;
    const top = 50 + (graphHeight - usedHeight) / 2;
    for (let index = 0; index < column.length; index++) {
      column[index].x = 80 + depth * columnGap;
      column[index].y = top + index * rowGap;
    }
  }
}

function nodeRadius(node) {
  if (node.kind === "target") {
    return 13;
  }
  if (node.kind === "dependency") {
    return 10;
  }
  if (node.kind === "seed") {
    return 8;
  }
  return 6;
}

function shortFile(file) {
  const parts = file.split("/");
  return parts.slice(Math.max(0, parts.length - 2)).join("/");
}

function nodeName(node) {
  if (node.kind === "target") {
    return node.symbol;
  }
  if (node.symbol === "<module>") {
    return shortFile(node.file);
  }
  return node.symbol;
}

function edgeKey(left, right) {
  return left < right ? `${left}|${right}` : `${right}|${left}`;
}

function pathNodes(node) {
  const path = (node.paths || [])[selectedPathIndex] || [];
  return path.map((id) => nodeById.get(id)).filter(Boolean);
}

function pathNodeSet(node) {
  return new Set(pathNodes(node).map((step) => step.id));
}

function pathEdgeSet(node) {
  const result = new Set();
  const path = (node.paths || [])[selectedPathIndex] || [];
  for (let index = 0; index + 1 < path.length; index++) {
    result.add(edgeKey(path[index], path[index + 1]));
  }
  return result;
}

function drawArrow(link, highlighted, dimmed) {
  const from = link.target;
  const to = link.source;
  const dx = to.x - from.x;
  const dy = to.y - from.y;
  const distance = Math.hypot(dx, dy);
  if (distance < 1) {
    return;
  }

  const ux = dx / distance;
  const uy = dy / distance;
  const startX = from.x + ux * (nodeRadius(from) + 2);
  const startY = from.y + uy * (nodeRadius(from) + 2);
  const endX = to.x - ux * (nodeRadius(to) + 4);
  const endY = to.y - uy * (nodeRadius(to) + 4);
  const color = highlighted ? "#f8fafc" : link.type ? "#60a5fa" : "#64748b";

  ctx.globalAlpha = dimmed ? 0.1 : highlighted ? 1 : 0.48;
  ctx.strokeStyle = color;
  ctx.fillStyle = color;
  ctx.lineWidth = highlighted ? 2.4 : 1.2;
  ctx.setLineDash(link.type ? [5, 5] : []);
  ctx.beginPath();
  ctx.moveTo(startX, startY);
  ctx.lineTo(endX, endY);
  ctx.stroke();
  ctx.setLineDash([]);

  const arrowLength = highlighted ? 9 : 7;
  const arrowWidth = highlighted ? 4.5 : 3.5;
  ctx.beginPath();
  ctx.moveTo(endX, endY);
  ctx.lineTo(
    endX - ux * arrowLength - uy * arrowWidth,
    endY - uy * arrowLength + ux * arrowWidth
  );
  ctx.lineTo(
    endX - ux * arrowLength + uy * arrowWidth,
    endY - uy * arrowLength - ux * arrowWidth
  );
  ctx.closePath();
  ctx.fill();
}

function drawNode(node, pathIds, neighbors, matches) {
  const onPath = pathIds?.has(node.id) ?? false;
  const related = selected && (node === selected || onPath || neighbors?.has(node.id));
  const dimmed = (selected && !related) || (filterText && !matches);
  const radius = nodeRadius(node);

  ctx.globalAlpha = dimmed ? 0.12 : 1;
  ctx.fillStyle = colors[node.kind] || colors.normal;
  ctx.strokeStyle = node === selected ? "#fff" : "#0f1117";
  ctx.lineWidth = node === selected ? 3 : 2;
  ctx.beginPath();
  ctx.arc(node.x, node.y, radius, 0, Math.PI * 2);
  ctx.fill();
  ctx.stroke();

  if (node === hovered && node !== selected) {
    ctx.strokeStyle = "#cbd5e1";
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.arc(node.x, node.y, radius + 4, 0, Math.PI * 2);
    ctx.stroke();
  }

  const showLabel =
    node.kind === "target" ||
    node.kind === "dependency" ||
    onPath ||
    node === selected ||
    node === hovered ||
    matches ||
    view.scale > 1.15;
  if (showLabel) {
    ctx.globalAlpha = dimmed ? 0.18 : 0.95;
    ctx.fillStyle = node.kind === "target" ? "#e9ddff" : "#d7dce7";
    ctx.font = node.kind === "target"
      ? "bold 13px ui-sans-serif, system-ui, sans-serif"
      : "11px ui-monospace, monospace";
    ctx.fillText(nodeName(node), node.x + radius + 6, node.y + 4);
  }

  if (onPath && selected) {
    const path = (selected.paths || [])[selectedPathIndex] || [];
    const index = path.indexOf(node.id);
    if (index >= 0) {
      ctx.globalAlpha = 1;
      ctx.fillStyle = "#f8fafc";
      ctx.strokeStyle = "#0f1117";
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.arc(node.x - radius - 4, node.y - radius - 4, 8, 0, Math.PI * 2);
      ctx.fill();
      ctx.stroke();
      ctx.fillStyle = "#111827";
      ctx.font = "bold 9px ui-sans-serif, system-ui, sans-serif";
      ctx.textAlign = "center";
      ctx.fillText(String(index + 1), node.x - radius - 4, node.y - radius - 1);
      ctx.textAlign = "start";
    }
  }
}

function draw() {
  ctx.clearRect(0, 0, width, height);
  ctx.save();
  ctx.translate(view.x, view.y);
  ctx.scale(view.scale, view.scale);

  const pathIds = selected ? pathNodeSet(selected) : null;
  const pathEdges = selected ? pathEdgeSet(selected) : null;
  const neighbors = selected ? adjacency.get(selected.id) : null;

  for (const link of links) {
    const key = edgeKey(link.source.id, link.target.id);
    const highlighted = pathEdges?.has(key) ?? false;
    const touchesSelected = selected && (
      link.source.id === selected.id ||
      link.target.id === selected.id
    );
    const dimmed = Boolean(selected && !highlighted && !touchesSelected);
    drawArrow(link, highlighted, dimmed);
  }

  for (const node of nodes) {
    const haystack = `${node.label} ${node.package} ${node.file} ${node.symbol}`.toLowerCase();
    const matches = Boolean(filterText && haystack.includes(filterText));
    drawNode(node, pathIds, neighbors, matches);
  }

  ctx.globalAlpha = 1;
  ctx.restore();
  requestAnimationFrame(draw);
}

function screenToWorld(px, py) {
  return {
    x: (px - view.x) / view.scale,
    y: (py - view.y) / view.scale
  };
}

function nodeAt(px, py) {
  const point = screenToWorld(px, py);
  let best = null;
  let bestDistance = 14 / view.scale;
  for (const node of nodes) {
    const distance = Math.hypot(node.x - point.x, node.y - point.y);
    if (distance < bestDistance) {
      best = node;
      bestDistance = distance;
    }
  }
  return best;
}

function fitNodes(focusNodes = nodes) {
  if (!focusNodes.length || !width || !height) {
    return;
  }
  const minX = Math.min(...focusNodes.map((node) => node.x));
  const maxX = Math.max(...focusNodes.map((node) => node.x));
  const minY = Math.min(...focusNodes.map((node) => node.y));
  const maxY = Math.max(...focusNodes.map((node) => node.y));
  const contentWidth = Math.max(120, maxX - minX + 220);
  const contentHeight = Math.max(120, maxY - minY + 120);
  view.scale = Math.min(1.4, width / contentWidth, height / contentHeight);
  view.x = width / 2 - ((minX + maxX) / 2) * view.scale;
  view.y = height / 2 - ((minY + maxY) / 2) * view.scale;
}

function focusNode(node) {
  view.scale = Math.max(view.scale, 1.25);
  view.x = width / 2 - node.x * view.scale;
  view.y = height / 2 - node.y * view.scale;
}

canvas.addEventListener("mousedown", (event) => {
  const node = nodeAt(event.offsetX, event.offsetY);
  moved = false;
  if (node) {
    dragging = node;
  } else {
    panning = true;
  }
  lastX = event.offsetX;
  lastY = event.offsetY;
});

window.addEventListener("mousemove", (event) => {
  const rect = canvas.getBoundingClientRect();
  const offsetX = event.clientX - rect.left;
  const offsetY = event.clientY - rect.top;
  if (dragging) {
    const point = screenToWorld(offsetX, offsetY);
    if (Math.hypot(offsetX - lastX, offsetY - lastY) > 2) {
      moved = true;
    }
    dragging.x = point.x;
    dragging.y = point.y;
  } else if (panning) {
    view.x += offsetX - lastX;
    view.y += offsetY - lastY;
    moved = true;
  } else {
    hovered = nodeAt(offsetX, offsetY);
    canvas.style.cursor = hovered ? "pointer" : "grab";
  }
  lastX = offsetX;
  lastY = offsetY;
});

window.addEventListener("mouseup", () => {
  if (dragging && !moved) {
    selectNode(dragging);
  }
  dragging = null;
  panning = false;
});

canvas.addEventListener("mouseleave", () => {
  hovered = null;
});

canvas.addEventListener("wheel", (event) => {
  event.preventDefault();
  const factor = event.deltaY < 0 ? 1.1 : 0.9;
  const point = screenToWorld(event.offsetX, event.offsetY);
  view.scale = Math.min(4, Math.max(0.15, view.scale * factor));
  view.x = event.offsetX - point.x * view.scale;
  view.y = event.offsetY - point.y * view.scale;
}, { passive: false });

function escapeHtml(value) {
  return String(value).replace(
    /[&<>"]/g,
    (character) => ({
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      "\"": "&quot;"
    })[character]
  );
}

function renderPath(node) {
  const path = pathNodes(node);
  if (path.length <= 1) {
    if (node.kind === "seed") {
      return `<h2>Impact path</h2><div class="value">This declaration changed and starts the impact flow.</div>`;
    }
    if (node.kind === "target" && node.details?.length) {
      return `<h2>Impact path</h2><div class="value">A build input changed this target directly.</div>`;
    }
    return "";
  }

  const heading = node.kind === "target" ? "Why it deploys" : "Why it is affected";
  const paths = node.paths || [];
  const options = paths.length > 1
    ? `<div class="path-options">${paths.map((_, index) => {
        const active = index === selectedPathIndex ? "active" : "";
        return `<button type="button" class="${active}" data-path-index="${index}">Path ${index + 1}</button>`;
      }).join("")}</div>`
    : "";
  const steps = path.map((step, index) => {
    const file = step.kind === "target" ? "deployment target" : step.file;
    const link = index > 0
      ? links.find((link) => {
          return link.source.id === step.id && link.target.id === path[index - 1].id;
        })
      : null;
    const reason = link
      ? `<div class="path-reason">↓ ${escapeHtml(link.detail)}${link.location ? `<small>${escapeHtml(link.location)}</small>` : ""}</div>`
      : "";
    return `<li>${reason}<span class="step-number">${index + 1}</span><button type="button" data-node-id="${step.id}"><strong>${escapeHtml(nodeName(step))}</strong><small>${escapeHtml(file)}</small></button></li>`;
  }).join("");
  return `<h2>${heading}</h2>${options}<ol class="path">${steps}</ol>`;
}

function renderRelations(node) {
  const consumers = links
    .filter((link) => link.target.id === node.id)
    .map((link) => ({ node: link.source, link }));
  const dependencies = links
    .filter((link) => link.source.id === node.id)
    .map((link) => ({ node: link.target, link }));

  const section = (title, relationships) => {
    if (!relationships.length) {
      return "";
    }
    const items = relationships
      .sort((left, right) => left.node.label.localeCompare(right.node.label))
      .slice(0, 30)
      .map(({ node: related, link }) => {
        const kind = link.type ? "type" : "runtime";
        const location = link.location ? ` · ${link.location}` : "";
        return `<li><button type="button" data-node-id="${related.id}"><span class="relation-title"><span>${escapeHtml(nodeName(related))}</span><span class="edge-kind">${kind}</span></span><small>${escapeHtml(link.detail + location)}</small></button></li>`;
      })
      .join("");
    return `<h2>${title}</h2><ul class="relation">${items}</ul>`;
  };

  return section("Can affect", consumers) + section("Depends on", dependencies);
}

function renderTargets() {
  const targets = nodes.filter((node) => node.kind === "target");
  if (!targets.length) {
    return "";
  }
  const items = targets.map((target) => {
    const active = selected?.id === target.id ? "active" : "";
    return `<li><button type="button" class="${active}" data-node-id="${target.id}"><span>${escapeHtml(target.symbol)}</span></button></li>`;
  }).join("");
  return `<h2>Affected targets</h2><ul class="targets">${items}</ul>`;
}

function bindSideButtons() {
  for (const button of side.querySelectorAll("button[data-node-id]")) {
    button.addEventListener("click", () => {
      const node = nodeById.get(Number(button.dataset.nodeId));
      if (node) {
        selectNode(node);
        focusNode(node);
      }
    });
  }
  for (const button of side.querySelectorAll("button[data-path-index]")) {
    button.addEventListener("click", () => {
      selectedPathIndex = Number(button.dataset.pathIndex);
      renderSide();
      fitNodes(pathNodes(selected));
    });
  }
}

function renderSide() {
  if (!selected) {
    side.innerHTML = `
      <h2>Reading the graph</h2>
      <div class="value">Arrows follow the impact of a change from a dependency to each consumer. Select any node to isolate its path and inspect both sides of the relationship.</div>
      ${renderTargets()}
      <p class="hint">Solid arrows are runtime relationships. Dashed blue arrows are type-only relationships. Drag to pan, scroll to zoom, or drag a node to reposition it.</p>
    `;
    bindSideButtons();
    return;
  }

  const details = selected.details?.length
    ? `<h2>What changed</h2><ul>${selected.details.map((detail) => `<li>${escapeHtml(detail)}</li>`).join("")}</ul>`
    : "";
  side.innerHTML = `
    <h2>Selected node</h2>
    <div class="node-title">${escapeHtml(nodeName(selected))}</div>
    <dl class="meta-grid">
      <dt>Kind</dt><dd><span class="kind">${escapeHtml(selected.kind)}</span></dd>
      <dt>Package</dt><dd>${escapeHtml(selected.package || "—")}</dd>
      <dt>File</dt><dd>${escapeHtml(selected.file || "—")}</dd>
      <dt>Symbol</dt><dd>${escapeHtml(selected.symbol)}</dd>
    </dl>
    ${details}
    ${renderPath(selected)}
    ${renderRelations(selected)}
    ${renderTargets()}
    <p class="hint">Arrows point from a dependency to the code it can affect. Select a path step or neighboring node to follow the relationship.</p>
  `;
  bindSideButtons();
}

function selectNode(node) {
  if (selected?.id !== node.id) {
    selectedPathIndex = 0;
  }
  selected = node;
  renderSide();
}

document.getElementById("filter").addEventListener("input", (event) => {
  filterText = event.target.value.trim().toLowerCase();
});

document.getElementById("filter").addEventListener("keydown", (event) => {
  if (event.key !== "Enter" || !filterText) {
    return;
  }
  const match = nodes.find((node) => {
    return `${node.label} ${node.package} ${node.file} ${node.symbol}`
      .toLowerCase()
      .includes(filterText);
  });
  if (match) {
    selectNode(match);
    focusNode(match);
  }
});

document.getElementById("fit").addEventListener("click", () => {
  fitNodes(selected && pathNodes(selected).length > 1 ? pathNodes(selected) : nodes);
});

document.getElementById("clear").addEventListener("click", () => {
  selected = null;
  selectedPathIndex = 0;
  filterText = "";
  document.getElementById("filter").value = "";
  fitNodes(nodes);
  renderSide();
});

window.addEventListener("resize", () => {
  resize();
});

resize();
if (nodes.length) {
  layout();
  fitNodes(nodes);
  const firstTarget = nodes.find((node) => {
    return node.kind === "target" && (node.paths || []).some((path) => path.length > 1);
  }) || nodes.find((node) => node.kind === "target");
  if (firstTarget) {
    selectNode(firstTarget);
  } else {
    renderSide();
  }
} else {
  document.getElementById("empty").style.display = "grid";
  renderSide();
}

const seeds = nodes.filter((node) => node.kind === "seed").length;
const targets = nodes.filter((node) => node.kind === "target").length;
document.getElementById("summary").textContent =
  `${DATA.scope} · ${DATA.task} · ${nodes.length} nodes · ${seeds} changed · ${targets} targets`;
requestAnimationFrame(draw);
</script>
</body>
</html>
"##;
