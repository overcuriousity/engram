const $ = (id) => document.getElementById(id);

function say(message, isError) {
  const el = $('status');
  el.hidden = !message;
  el.textContent = message || '';
  el.classList.toggle('error', !!isError);
}

// Filled in by later tasks. Wired now so the shell is verifiably loaded.
$('capture').addEventListener('click', () => say('not wired yet'));
