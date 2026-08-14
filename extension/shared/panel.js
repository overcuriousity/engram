const $ = (id) => document.getElementById(id);

function say(message, isError) {
  const el = $('status');
  el.hidden = !message;
  el.textContent = message || '';
  el.classList.toggle('error', !!isError);
}

/// Trailing slashes would double up in every URL built from this.
function cleanOrigin(raw) {
  return raw.trim().replace(/\/+$/, '');
}

async function ensurePaired() {
  if (await engramApi.config()) return true;

  const typed = prompt('engram address (for example https://engram.example)');
  if (!typed) return false;
  try {
    say('Pairing…');
    const origin = await engramPair.pair(cleanOrigin(typed));
    say('Paired with ' + origin + '.');
    return true;
  } catch (e) {
    say(e.message, true);
    return false;
  }
}

$('capture').addEventListener('click', async () => {
  if (!(await ensurePaired())) return;
  say('not wired yet');
});
