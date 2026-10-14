/* Éditeur de workflow simulé — calqué sur examples/workflows/slack-to-gmail-notion.json */
(function () {
  const NODES = {
    entry: {
      kind: "Trigger · manual",
      fields: [["trigger_type", "manual"], ["déclenchement", "au clic « Exécuter »"]]
    },
    slack_read: {
      kind: "Action · Slack",
      fields: [["action_type", "slack.read_channel"], ["channel", "C123DEPLOYS"], ["limit", "1"]]
    },
    gmail_send: {
      kind: "Action · Gmail",
      fields: [["action_type", "gmail.send_email"], ["to", "ops@example.com"],
               ["subject", "Résumé Slack #deploys"],
               ["body", "Dernier message #deploys : <span class='tmpl'>{{ slack_read.messages.0.text }}</span>"]]
    },
    notion_create: {
      kind: "Action · Notion",
      fields: [["action_type", "notion.create_page"], ["database_id", "db-archive-0001"],
               ["Name", "<span class='tmpl'>{{ slack_read.messages.0.text }}</span>"],
               ["EmailId", "<span class='tmpl'>{{ gmail_send.id }}</span>"]]
    }
  };
  const ORDER = ["entry", "slack_read", "gmail_send", "notion_create"];
  const EDGES = [["entry", "slack_read"], ["slack_read", "gmail_send"], ["gmail_send", "notion_create"]];

  const canvas = document.getElementById("canvas");
  const wires = document.getElementById("wires");
  if (!canvas) return;
  const nodeEl = (id) => canvas.querySelector('.wf-node[data-id="' + id + '"]');

  function drawWires() {
    wires.innerHTML = "";
    const cb = canvas.getBoundingClientRect();
    EDGES.forEach(([from, to]) => {
      const a = nodeEl(from).getBoundingClientRect();
      const b = nodeEl(to).getBoundingClientRect();
      const x1 = a.left - cb.left + a.width / 2, y1 = a.bottom - cb.top;
      const x2 = b.left - cb.left + b.width / 2, y2 = b.top - cb.top;
      const my = (y1 + y2) / 2;
      const p = document.createElementNS("http://www.w3.org/2000/svg", "path");
      p.setAttribute("d", "M " + x1 + " " + y1 + " C " + x1 + " " + my + ", " + x2 + " " + my + ", " + x2 + " " + y2);
      p.dataset.from = from; p.dataset.to = to;
      wires.appendChild(p);
    });
  }
  window.addEventListener("resize", drawWires);
  window.addEventListener("load", drawWires);
  setTimeout(drawWires, 60);

  const inspEmpty = document.getElementById("inspEmpty");
  const inspBody = document.getElementById("inspBody");
  const inspKind = document.getElementById("inspKind");
  const inspFields = document.getElementById("inspFields");

  function select(id) {
    canvas.querySelectorAll(".wf-node").forEach((n) => n.classList.toggle("sel", n.dataset.id === id));
    const d = NODES[id];
    inspEmpty.style.display = "none";
    inspBody.style.display = "block";
    inspKind.innerHTML = d.kind;
    inspFields.innerHTML = d.fields.map(([k, v]) =>
      '<div class="field"><label>' + k + '</label><div class="val">' + v + "</div></div>").join("");
  }
  canvas.querySelectorAll(".wf-node").forEach((n) => {
    n.addEventListener("click", () => select(n.dataset.id));
  });
  select("gmail_send");

  const runBtn = document.getElementById("runBtn");
  const resetBtn = document.getElementById("resetBtn");
  const runStatus = document.getElementById("runStatus");
  let running = false;

  function resetRun() {
    running = false;
    canvas.querySelectorAll(".wf-node").forEach((n) => {
      n.classList.remove("running", "done");
      n.querySelector(".status").innerHTML = "";
    });
    wires.querySelectorAll("path").forEach((p) => p.classList.remove("active"));
    runStatus.innerHTML = "prêt.";
    runBtn.disabled = false;
  }
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  async function runWorkflow() {
    if (running) return;
    resetRun();
    running = true; runBtn.disabled = true;
    runStatus.innerHTML = "run enfilé dans RabbitMQ…";
    await sleep(600);
    runStatus.innerHTML = "worker → moteur DAG en cours…";
    for (let i = 0; i < ORDER.length; i++) {
      const id = ORDER[i];
      const el = nodeEl(id);
      select(id);
      el.classList.add("running");
      el.querySelector(".status").innerHTML = '<span class="spinner"></span>';
      runStatus.innerHTML = 'exécution : <b style="color:var(--amber)">' + id + "</b>";
      await sleep(850);
      el.classList.remove("running");
      el.classList.add("done");
      el.querySelector(".status").textContent = "✓";
      wires.querySelectorAll('path[data-from="' + id + '"]').forEach((p) => p.classList.add("active"));
      runStatus.innerHTML = "checkpoint persisté : <b>" + id + "</b> ✓";
      await sleep(350);
    }
    runStatus.innerHTML = "<b>run terminé</b> — 4 nodes ✓ (état en base)";
    running = false; runBtn.disabled = false;
  }
  runBtn.addEventListener("click", runWorkflow);
  resetBtn.addEventListener("click", resetRun);
})();
