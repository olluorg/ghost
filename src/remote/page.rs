//! Страница помощника. Отдаётся одним куском: отдельный фронтенд здесь только
//! добавил бы сборку, а функций всего три — смотреть, слушать, писать.

pub const HTML: &str = r##"<!doctype html>
<html lang="ru">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ghost · помощник</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; background: #0d0d11; color: #e6e6ee;
         font: 14px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; }
  header { display: flex; gap: 12px; align-items: center;
           padding: 8px 12px; background: #16161d; border-bottom: 1px solid #24242e; }
  #dot { width: 9px; height: 9px; border-radius: 50%; background: #555; flex: none; }
  #dot.on { background: #64c88a; }
  button { background: #24242e; color: #e6e6ee; border: 1px solid #33333f;
           border-radius: 6px; padding: 6px 12px; cursor: pointer; font: inherit; }
  button:hover { background: #2d2d3a; }
  main { display: flex; height: calc(100vh - 45px); }
  #screen { flex: 1; min-width: 0; object-fit: contain; background: #000; }
  aside { width: 320px; flex: none; display: flex; flex-direction: column;
          border-left: 1px solid #24242e; }
  #log { flex: 1; overflow-y: auto; padding: 10px; margin: 0; list-style: none; }
  #log li { margin-bottom: 8px; padding: 7px 9px; background: #1b1b24;
            border-radius: 6px; border-left: 2px solid #4a4a58; word-wrap: break-word;
            white-space: pre-wrap; }
  #log li.mine { border-left-color: #5bc8b0; }
  #log li.llm  { border-left-color: #d8b070; background: #241f17; }
  #log li.said { border-left-color: #6a8fbf; background: #16181d; color: #b9c2cf; }
  #log b { color: #9aa4b4; display: block; font-size: 11px; margin-bottom: 3px;
           text-transform: lowercase; font-weight: 600; }
  form { display: flex; gap: 6px; padding: 10px; border-top: 1px solid #24242e; }
  input { flex: 1; min-width: 0; background: #16161d; color: #e6e6ee;
          border: 1px solid #33333f; border-radius: 6px; padding: 8px; font: inherit; }
  .hint { color: #7a7a8a; font-size: 12px; padding: 0 10px 10px; }
  @media (max-width: 780px) { main { flex-direction: column; height: auto; }
                              aside { width: auto; border-left: 0; border-top: 1px solid #24242e; }
                              #log { max-height: 40vh; } }
</style>

<header>
  <span id="dot"></span>
  <span id="state">подключение…</span>
  <button id="sound">включить звук</button>
</header>

<main>
  <img id="screen" alt="экран">
  <aside>
    <ul id="log"></ul>
    <p class="hint">сообщение появится в оверлее у собеседника</p>
    <form id="form">
      <input id="text" placeholder="подсказка…" autocomplete="off">
      <button type="submit">отправить</button>
    </form>
  </aside>
</main>

<script>
const token = new URLSearchParams(location.search).get('t') || '';
const dot = document.getElementById('dot');
const state = document.getElementById('state');
const img = document.getElementById('screen');
const log = document.getElementById('log');

// --- экран: опрос кадров вместо потока. При двух-трёх кадрах в секунду это
// проще многочастного ответа и не ломается при обрыве.
async function tick() {
  try {
    const r = await fetch('/frame.jpg?t=' + token + '&n=' + Date.now());
    if (r.ok) {
      const url = URL.createObjectURL(await r.blob());
      img.onload = () => URL.revokeObjectURL(url);
      img.src = url;
    }
  } catch (e) { /* сеть моргнула — просто ждём следующий кадр */ }
  setTimeout(tick, 300);
}
tick();

// --- звук: 16 кГц моно, 16 бит. Браузер не даёт запустить воспроизведение без
// действия человека, поэтому есть кнопка.
let audio = null, nextAt = 0;
document.getElementById('sound').onclick = (e) => {
  audio = new AudioContext({ sampleRate: 16000 });
  audio.resume();
  e.target.remove();
};

function play(bytes) {
  if (!audio) return;
  const pcm = new Int16Array(bytes);
  const buf = audio.createBuffer(1, pcm.length, 16000);
  const ch = buf.getChannelData(0);
  for (let i = 0; i < pcm.length; i++) ch[i] = pcm[i] / 32768;
  const src = audio.createBufferSource();
  src.buffer = buf;
  src.connect(audio.destination);
  // Небольшой запас впереди: без него куски стыкуются с щелчками.
  const now = audio.currentTime;
  if (nextAt < now + 0.08) nextAt = now + 0.08;
  // Верхняя граница обязательна: поток чуть быстрее реального времени уводит
  // nextAt вперёд, и задержка растёт без предела, пока звук не отстанет на
  // секунды. Ушли далеко — начинаем заново от «сейчас».
  if (nextAt > now + 0.6) nextAt = now + 0.15;
  src.start(nextAt);
  nextAt += buf.duration;
}

// --- чат и звук идут по одному сокету
let ws = null;
function connect() {
  ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws?t=${token}`);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => { dot.classList.add('on'); state.textContent = 'на связи'; };
  ws.onmessage = (e) => {
    if (e.data instanceof ArrayBuffer) { play(e.data); return; }
    // Текстом приходит лента: что распознано и что ответила модель. Без неё
    // помощник не знает, что уже сказано, и дублирует подсказки.
    try {
      const m = JSON.parse(e.data);
      add(m.kind === 'llm' ? 'llm' : 'said', m.who, m.text);
    } catch (err) { /* мусорный кадр не повод рвать соединение */ }
  };
  ws.onclose = () => {
    dot.classList.remove('on');
    state.textContent = 'связь потеряна, переподключаюсь…';
    setTimeout(connect, 1500);
  };
}
connect();

function add(cls, who, text) {
  const li = document.createElement('li');
  li.className = cls;
  if (who) { const b = document.createElement('b'); b.textContent = who; li.appendChild(b); }
  li.appendChild(document.createTextNode(text));
  log.appendChild(li);
  log.scrollTop = log.scrollHeight;
}

document.getElementById('form').onsubmit = (e) => {
  e.preventDefault();
  const input = document.getElementById('text');
  const text = input.value.trim();
  if (!text || !ws || ws.readyState !== 1) return;
  ws.send(text);
  add('mine', 'вы', text);
  input.value = '';
};
</script>
</html>"##;
