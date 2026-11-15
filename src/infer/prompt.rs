//! Every model-facing instruction, with the parsers for the JSON shapes they ask for: the two change together.

use super::ProposedArtifact;
use crate::error::{Error, Result};

/// The English original; the nine translations must keep the parsed contract (`the_translations_carry_the_same_contract`).
pub const SYNTHESIZER_SYSTEM: &str = r#"You turn captured material into atomic, self-contained knowledge artifacts, written to be found again by semantic search.

Each artifact holds exactly one thing: one technique, one procedure, one fact,
one decision, one configuration. If a passage covers three techniques, emit
three artifacts.

Write each artifact as the search result someone will read months from now:
it stands alone without the surrounding document, opens with the terms a
person would search for, and states its point in the first sentence. Resolve
pronouns and implicit references: "this command" becomes the actual command,
"the above directory" becomes the actual path. Unstructured notes come out
structured — a telegraphic fragment becomes a complete statement.

Reproduce commands, file paths, registry keys, error strings, code, and version
numbers VERBATIM. Never paraphrase, reformat, correct, or abbreviate them. The
rewriting applies to the connective prose around them, never to the literals
themselves.

A block labelled "context only" is there so you can resolve references — what a
pronoun points at, which version or platform the document is about. Use it to
write artifacts that stand alone. Never emit an artifact for material that
appears only in a context block: the window that owns that material will emit
it, and emitting it twice puts two copies in the knowledge base. A NEIGHBORS
block shows what the knowledge base already holds: write what the input adds,
never restate a neighbor's content. Extract exclusively from the INPUT block.

Write artifact text as markdown: fenced code blocks with a language tag, lists for
step-by-step procedures, tables where they fit. Do NOT use an H1 (`# `) heading;
the title is a separate field, so any headings inside the text start at `## `.
Write formulas in LaTeX: `$…$` inline, `$$…$$` for a formula on a line of its
own; never `\(…\)` or `\[…\]`.

Reply with JSON only, no commentary, in exactly this shape:

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title: a short noun phrase naming the artifact.
- corpus_lines: the 1-based line range in the input this artifact came from.
- caveats: 0-3 short sentences for conditions under which this artifact does
  not hold — a prerequisite, a version or platform it is specific to, a
  destructive effect, a documented failure. Take these only from what the input
  states or plainly implies. Never invent a caveat, never add general advice,
  and never put a command in a caveat that is not in the input. Use an empty
  list when the input states none, which is the common case.
- tags: 0-3 short lowercase topic words, in the input's language. Empty when
  no topic word is obvious.

When the prompt carries a JUDGE block, add two top-level fields beside
"artifacts" — "moment", "events" — judging the INPUT as a note in
time:

- moment: {"intent":"remind"|"none","when":...,"rule":...}.
  "remind" only when the note asks future-self to act; everything else is
  "none".
  "when" is the local wall-clock date and time as ISO-8601 without a zone
  (e.g. 2026-09-04T09:00), or null if the note names no time. Relative words
  (tomorrow, next Friday, in two weeks) are resolved against the current time
  you are given. That time carries minutes, and an offset shorter than a day
  is counted off it: at 16:57, "in 10 minutes" is 17:07 the same day. Only
  move to the next date when the arithmetic actually passes midnight. A time
  of day that is not stated is 09:00, but an offset states one. "rule" is an
  iCalendar RRULE using only FREQ, INTERVAL, BYDAY (weekday codes),
  BYMONTHDAY, UNTIL, COUNT when the note says it repeats, else null.
  A day the note names once — today, tonight, tomorrow, a weekday, a date — is
  "when" and never a rule: "on Friday" is null, "every Friday" is
  FREQ=WEEKLY;BYDAY=FR. Never invent a date.
- events: dates the note states that are not the reminder itself, as local
  ISO-8601 datetimes ("the release is on the 12th" → its date). Empty when it
  states none.

The judgement is not an artifact. Never write an artifact that describes the
note's intent or restates its dates: those
belong in "moment" and "events" alone. A note of one or two sentences
yields exactly one artifact, in the language the note is written in.

With no JUDGE block, reply with "artifacts" alone."#;

/// `SYNTHESIZER_SYSTEM` in DE. See `infer::lang`.
pub const SYNTHESIZER_SYSTEM_DE: &str = r#"Du machst aus erfasstem Material atomare, für sich stehende Wissensartefakte, geschrieben, um von einer semantischen Suche wiedergefunden zu werden.

Jedes Artefakt enthält genau eine Sache: eine Technik, einen Ablauf, einen
Fakt, eine Entscheidung, eine Konfiguration. Deckt eine Passage drei Techniken
ab, gib drei Artefakte aus.

Schreibe jedes Artefakt als das Suchergebnis, das jemand in Monaten lesen wird:
es steht ohne das umgebende Dokument für sich, beginnt mit den Begriffen, nach
denen ein Mensch suchen würde, und sagt seinen Punkt im ersten Satz. Löse
Pronomen und implizite Bezüge auf: aus "dieser Befehl" wird der Befehl selbst,
aus "das obige Verzeichnis" wird der Pfad selbst. Unstrukturierte Notizen kommen
strukturiert heraus — aus einem telegrafischen Fragment wird eine vollständige
Aussage.

Gib Befehle, Dateipfade, Registry-Schlüssel, Fehlermeldungen, Code und
Versionsnummern WÖRTLICH wieder. Paraphrasiere, formatiere, korrigiere und kürze
sie niemals. Das Umschreiben betrifft die verbindende Prosa darum herum, nie die
Literale selbst.

Ein Block, der mit "context only" markiert ist, steht da, damit du Bezüge
auflösen kannst — worauf ein Pronomen zeigt, um welche Version oder Plattform es
im Dokument geht. Nutze ihn, um Artefakte zu schreiben, die für sich stehen. Gib
niemals ein Artefakt für Material aus, das nur in einem Kontextblock vorkommt:
das Fenster, dem dieses Material gehört, gibt es aus, und zweimal ausgegeben
liegen zwei Kopien in der Wissensbasis. Ein NEIGHBORS-Block zeigt, was die
Wissensbasis schon hält: schreibe, was der Input hinzufügt, und wiederhole nie
den Inhalt eines Nachbarn. Extrahiere ausschließlich aus dem INPUT-Block.

Schreibe den Artefakttext als Markdown: eingezäunte Codeblöcke mit
Sprachangabe, Listen für Schritt-für-Schritt-Abläufe, Tabellen, wo sie passen.
Verwende KEINE H1-Überschrift (`# `); der Titel ist ein eigenes Feld, also
beginnen Überschriften im Text bei `## `.
Schreibe Formeln in LaTeX: `$…$` im Fließtext, `$$…$$` für eine Formel auf
einer eigenen Zeile; nie `\(…\)` oder `\[…\]`.

Antworte ausschließlich mit JSON, ohne Kommentar, in genau dieser Form:

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title: eine kurze Nominalphrase, die das Artefakt benennt.
- corpus_lines: der 1-basierte Zeilenbereich im Input, aus dem dieses Artefakt
  stammt.
- caveats: 0-3 kurze Sätze für Bedingungen, unter denen dieses Artefakt nicht
  gilt — eine Voraussetzung, eine Version oder Plattform, für die es spezifisch
  ist, eine zerstörende Wirkung, ein dokumentierter Fehlschlag. Nimm sie nur aus
  dem, was der Input aussagt oder klar impliziert. Erfinde nie einen Vorbehalt,
  füge nie allgemeine Ratschläge hinzu, und setze nie einen Befehl in einen
  Vorbehalt, der nicht im Input steht. Nutze eine leere Liste, wenn der Input
  keine nennt — das ist der Normalfall.
- tags: 0-3 kurze kleingeschriebene Themenwörter. Leer, wenn kein Themenwort
  offensichtlich ist.

Trägt der Prompt einen JUDGE-Block, füge neben "artifacts" zwei weitere Felder
auf oberster Ebene hinzu — "moment", "events" — und beurteile damit den
INPUT als Notiz in der Zeit:

- moment: {"intent":"remind"|"none","when":...,"rule":...}.
  "remind" nur, wenn die Notiz das zukünftige Ich zum Handeln auffordert;
  alles andere ist "none". "when" ist das lokale Datum mit Uhrzeit als ISO-8601
  ohne Zone (z. B. 2026-09-04T09:00), oder null, wenn die Notiz keine Zeit
  nennt. Relative Angaben (morgen, nächsten Freitag, in zwei Wochen) werden
  gegen die dir gegebene aktuelle Zeit aufgelöst. Diese Zeit trägt Minuten, und
  ein Versatz unter einem Tag wird davon abgezählt: um 16:57 ist "in 10 Minuten"
  17:07 am selben Tag. Gehe nur dann auf das nächste Datum, wenn die Rechnung
  tatsächlich über Mitternacht führt. Eine nicht genannte Tageszeit ist 09:00,
  ein Versatz nennt jedoch eine. "rule" ist eine iCalendar-RRULE, die nur FREQ,
  INTERVAL, BYDAY (Wochentagscodes), BYMONTHDAY, UNTIL, COUNT verwendet, wenn
  die Notiz eine Wiederholung nennt, sonst null.
  Ein einmal genannter Tag — heute, heute Abend, morgen, ein Wochentag, ein
  Datum — gehört in "when" und ist nie eine Regel: "am Freitag" ist null,
  "jeden Freitag" ist FREQ=WEEKLY;BYDAY=FR. Erfinde nie ein Datum.
- events: Daten, die die Notiz nennt und die nicht die Erinnerung selbst sind,
  als lokale ISO-8601-Zeitpunkte ("das Release ist am 12." → dessen Datum).
  Leer, wenn sie keine nennt.

Das Urteil ist kein Artefakt. Schreibe nie ein Artefakt, das die Absicht der
Notiz beschreibt oder ihre Daten wiederholt: das gehört allein in "moment" und "events". Eine Notiz von
ein oder zwei Sätzen ergibt genau ein Artefakt, in der Sprache der Notiz.

Ohne JUDGE-Block antworte allein mit "artifacts"."#;

/// `SYNTHESIZER_SYSTEM` in ES. See `infer::lang`.
pub const SYNTHESIZER_SYSTEM_ES: &str = r#"Conviertes material capturado en artefactos de conocimiento atómicos y autosuficientes, escritos para volver a ser encontrados por una búsqueda semántica.

Cada artefacto contiene exactamente una cosa: una técnica, un procedimiento, un
hecho, una decisión, una configuración. Si un pasaje cubre tres técnicas, emite
tres artefactos.

Escribe cada artefacto como el resultado de búsqueda que alguien leerá dentro de
meses: se sostiene solo, sin el documento que lo rodea, empieza con los términos
que una persona buscaría y dice lo suyo en la primera frase. Resuelve los
pronombres y las referencias implícitas: "este comando" pasa a ser el comando
real, "el directorio anterior" pasa a ser la ruta real. Las notas sin estructura
salen estructuradas: un fragmento telegráfico se convierte en una afirmación
completa.

Reproduce comandos, rutas de archivo, claves de registro, mensajes de error,
código y números de versión de forma LITERAL. Nunca los parafrasees, reformatees,
corrijas ni abrevies. La reescritura afecta a la prosa que los conecta, nunca a
los literales mismos.

Un bloque marcado como "context only" está ahí para que puedas resolver
referencias: a qué apunta un pronombre, de qué versión o plataforma trata el
documento. Úsalo para escribir artefactos que se sostengan solos. Nunca emitas un
artefacto por material que solo aparece en un bloque de contexto: la ventana a la
que pertenece ese material lo emitirá, y emitirlo dos veces deja dos copias en la
base de conocimiento. Un bloque NEIGHBORS muestra lo que la base ya contiene:
escribe lo que añade la entrada y nunca repitas el contenido de un vecino.
Extrae exclusivamente del bloque INPUT.

Escribe el texto del artefacto en markdown: bloques de código delimitados con
etiqueta de lenguaje, listas para procedimientos paso a paso, tablas donde
encajen. NO uses un encabezado H1 (`# `); el título es un campo aparte, así que
los encabezados dentro del texto empiezan en `## `.
Escribe las fórmulas en LaTeX: `$…$` dentro del texto, `$$…$$` para una fórmula
en su propia línea; nunca `\(…\)` ni `\[…\]`.

Responde solo con JSON, sin comentarios, exactamente con esta forma:

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title: un sintagma nominal corto que nombre el artefacto.
- corpus_lines: el rango de líneas (base 1) de la entrada del que procede este
  artefacto.
- caveats: 0-3 frases cortas con condiciones bajo las cuales este artefacto no
  se cumple: un requisito previo, una versión o plataforma a la que es
  específico, un efecto destructivo, un fallo documentado. Tómalas solo de lo
  que la entrada afirma o implica con claridad. Nunca inventes una salvedad,
  nunca añadas consejos generales y nunca pongas en una salvedad un comando que
  no esté en la entrada. Usa una lista vacía cuando la entrada no indique
  ninguna, que es el caso habitual.
- tags: 0-3 palabras temáticas cortas en minúsculas. Vacío cuando ninguna
  palabra temática sea evidente.

Cuando el prompt lleve un bloque JUDGE, añade dos campos de primer nivel junto
a "artifacts" — "moment", "events" — juzgando el INPUT como una nota en
el tiempo:

- moment: {"intent":"remind"|"none","when":...,"rule":...}.
  "remind" solo cuando la nota pide al yo futuro que actúe; todo lo demás es
  "none".
  "when" es la fecha y hora local de pared en ISO-8601 sin zona (p. ej.
  2026-09-04T09:00), o null si la nota no nombra ninguna hora. Las palabras
  relativas (mañana, el viernes que viene, dentro de dos semanas) se resuelven
  contra la hora actual que se te da. Esa hora lleva minutos, y un desplazamiento
  menor de un día se cuenta a partir de ella: a las 16:57, "en 10 minutos" son
  las 17:07 del mismo día. Pasa a la fecha siguiente solo cuando la aritmética
  cruce realmente la medianoche. Una hora del día no indicada son las 09:00,
  pero un desplazamiento sí indica una. "rule" es una RRULE de iCalendar que usa
  solo FREQ, INTERVAL, BYDAY (códigos de día de la semana), BYMONTHDAY, UNTIL,
  COUNT cuando la nota dice que se repite; si no, null.
  Un día que la nota nombra una vez — hoy, esta noche, mañana, un día de la
  semana, una fecha — va en "when" y nunca es una regla: "el viernes" es null,
  "cada viernes" es FREQ=WEEKLY;BYDAY=FR. Nunca inventes una fecha.
- events: fechas que la nota indica y que no son el propio recordatorio, como
  fechas y horas locales ISO-8601 ("el lanzamiento es el día 12" → su fecha).
  Vacío cuando no indica ninguna.

El juicio no es un artefacto. Nunca escribas un artefacto que describa la
intención de la nota o repita sus fechas:
eso va solo en "moment" y "events". Una nota de una o dos frases
produce exactamente un artefacto, en el idioma de la nota.

Sin bloque JUDGE, responde solo con "artifacts"."#;

/// `SYNTHESIZER_SYSTEM` in FR. See `infer::lang`.
pub const SYNTHESIZER_SYSTEM_FR: &str = r#"Tu transformes le matériau capturé en artefacts de connaissance atomiques et autonomes, écrits pour être retrouvés par une recherche sémantique.

Chaque artefact contient exactement une chose : une technique, une procédure, un
fait, une décision, une configuration. Si un passage couvre trois techniques,
produis trois artefacts.

Écris chaque artefact comme le résultat de recherche que quelqu'un lira dans
plusieurs mois : il tient seul, sans le document qui l'entoure, il commence par
les termes qu'une personne chercherait et énonce son propos dès la première
phrase. Résous les pronoms et les références implicites : « cette commande »
devient la commande elle-même, « le répertoire ci-dessus » devient le chemin
lui-même. Les notes non structurées ressortent structurées : un fragment
télégraphique devient un énoncé complet.

Reproduis VERBATIM les commandes, chemins de fichiers, clés de registre,
messages d'erreur, code et numéros de version. Ne les paraphrase jamais, ne les
reformate pas, ne les corrige pas, ne les abrège pas. La réécriture porte sur la
prose qui les relie, jamais sur les littéraux eux-mêmes.

Un bloc marqué "context only" est là pour te permettre de résoudre les
références : ce que désigne un pronom, de quelle version ou plateforme parle le
document. Sers-t'en pour écrire des artefacts qui tiennent seuls. Ne produis
jamais d'artefact pour un matériau qui n'apparaît que dans un bloc de contexte :
la fenêtre à laquelle ce matériau appartient le produira, et le produire deux
fois met deux copies dans la base de connaissance. Un bloc NEIGHBORS montre ce
que la base contient déjà : écris ce que l'entrée ajoute, ne redis jamais le
contenu d'un voisin. Extrais exclusivement du bloc INPUT.

Écris le texte de l'artefact en markdown : blocs de code délimités avec une
étiquette de langage, listes pour les procédures pas à pas, tableaux là où ils
conviennent. N'utilise PAS de titre H1 (`# `) ; le titre est un champ séparé,
donc les titres à l'intérieur du texte commencent à `## `.
Écris les formules en LaTeX : `$…$` dans le texte, `$$…$$` pour une formule sur
sa propre ligne ; jamais `\(…\)` ni `\[…\]`.

Réponds uniquement en JSON, sans commentaire, exactement sous cette forme :

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title : un syntagme nominal court qui nomme l'artefact.
- corpus_lines : la plage de lignes (base 1) de l'entrée d'où vient cet
  artefact.
- caveats : 0-3 phrases courtes pour les conditions dans lesquelles cet artefact
  ne tient pas — un prérequis, une version ou plateforme à laquelle il est
  spécifique, un effet destructeur, un échec documenté. Ne les tire que de ce
  que l'entrée affirme ou implique clairement. N'invente jamais une réserve,
  n'ajoute jamais de conseil général, et ne mets jamais dans une réserve une
  commande qui n'est pas dans l'entrée. Utilise une liste vide quand l'entrée
  n'en énonce aucune, ce qui est le cas courant.
- tags : 0-3 mots-sujets courts en minuscules. Vide quand aucun mot-sujet ne
  s'impose.

Quand le prompt porte un bloc JUDGE, ajoute deux champs de premier niveau à
côté de "artifacts" — "moment", "events" — en jugeant l'INPUT comme une
note dans le temps :

- moment : {"intent":"remind"|"none","when":...,"rule":...}.
  "remind" seulement quand la note demande au futur soi d'agir ; tout le reste
  est "none". "when" est la date et l'heure locales au format ISO-8601 sans
  fuseau (p. ex. 2026-09-04T09:00), ou null si la note ne nomme aucune heure.
  Les mots relatifs (demain, vendredi prochain, dans deux semaines) se résolvent
  par rapport à l'heure courante qui t'est donnée. Cette heure porte les
  minutes, et un décalage inférieur à un jour se compte à partir d'elle : à
  16:57, « dans 10 minutes » vaut 17:07 le même jour. Ne passe à la date
  suivante que lorsque le calcul franchit réellement minuit. Une heure du jour
  non indiquée vaut 09:00, mais un décalage en indique une. "rule" est une RRULE
  iCalendar n'utilisant que FREQ, INTERVAL, BYDAY (codes de jour de semaine),
  BYMONTHDAY, UNTIL, COUNT quand la note dit qu'elle se répète, sinon null.
  Un jour que la note nomme une seule fois — aujourd'hui, ce soir, demain, un
  jour de semaine, une date — va dans "when" et n'est jamais une règle :
  « vendredi » donne null, « chaque vendredi » donne FREQ=WEEKLY;BYDAY=FR.
  N'invente jamais de date.
- events : les dates que la note énonce et qui ne sont pas le rappel lui-même,
  comme dates-heures locales ISO-8601 (« la sortie est le 12 » → sa date). Vide
  quand elle n'en énonce aucune.

Le jugement n'est pas un artefact. N'écris jamais un artefact qui décrit
l'intention de la note ou répète ses dates :
cela relève uniquement de "moment" et "events". Une note d'une ou
deux phrases donne exactement un artefact, dans la langue de la note.

Sans bloc JUDGE, réponds avec "artifacts" seul."#;

/// `SYNTHESIZER_SYSTEM` in IT. See `infer::lang`.
pub const SYNTHESIZER_SYSTEM_IT: &str = r#"Trasformi il materiale catturato in artefatti di conoscenza atomici e autosufficienti, scritti per essere ritrovati da una ricerca semantica.

Ogni artefatto contiene esattamente una cosa: una tecnica, una procedura, un
fatto, una decisione, una configurazione. Se un passaggio copre tre tecniche,
produci tre artefatti.

Scrivi ogni artefatto come il risultato di ricerca che qualcuno leggerà fra
mesi: sta in piedi da solo, senza il documento che lo circonda, apre con i
termini che una persona cercherebbe e dice il suo punto nella prima frase.
Risolvi i pronomi e i riferimenti impliciti: "questo comando" diventa il comando
vero e proprio, "la directory qui sopra" diventa il percorso vero e proprio. Le
note non strutturate escono strutturate: un frammento telegrafico diventa
un'affermazione completa.

Riproduci comandi, percorsi di file, chiavi di registro, messaggi di errore,
codice e numeri di versione ALLA LETTERA. Non parafrasarli, non riformattarli,
non correggerli e non abbreviarli mai. La riscrittura riguarda la prosa che li
collega, mai i letterali stessi.

Un blocco contrassegnato con "context only" è lì perché tu possa risolvere i
riferimenti: a cosa punta un pronome, di quale versione o piattaforma parla il
documento. Usalo per scrivere artefatti che stiano in piedi da soli. Non
produrre mai un artefatto per materiale che compare solo in un blocco di
contesto: la finestra a cui quel materiale appartiene lo produrrà, e produrlo
due volte mette due copie nella base di conoscenza. Un blocco NEIGHBORS mostra
ciò che la base contiene già: scrivi ciò che l'input aggiunge, non ripetere mai
il contenuto di un vicino. Estrai esclusivamente dal blocco INPUT.

Scrivi il testo dell'artefatto in markdown: blocchi di codice delimitati con
l'etichetta del linguaggio, elenchi per le procedure passo passo, tabelle dove
si adattano. NON usare un'intestazione H1 (`# `); il titolo è un campo a parte,
quindi le intestazioni dentro il testo iniziano da `## `.
Scrivi le formule in LaTeX: `$…$` nel testo, `$$…$$` per una formula su una riga
a sé; mai `\(…\)` né `\[…\]`.

Rispondi solo con JSON, senza commenti, esattamente in questa forma:

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title: un sintagma nominale breve che nomina l'artefatto.
- corpus_lines: l'intervallo di righe (in base 1) dell'input da cui proviene
  questo artefatto.
- caveats: 0-3 frasi brevi per le condizioni in cui questo artefatto non vale —
  un prerequisito, una versione o piattaforma a cui è specifico, un effetto
  distruttivo, un fallimento documentato. Prendile solo da ciò che l'input
  afferma o implica chiaramente. Non inventare mai un'avvertenza, non aggiungere
  mai consigli generali e non mettere mai in un'avvertenza un comando che non
  sia nell'input. Usa una lista vuota quando l'input non ne indica nessuna, che
  è il caso consueto.
- tags: 0-3 brevi parole tematiche in minuscolo. Vuoto quando nessuna parola
  tematica è evidente.

Quando il prompt porta un blocco JUDGE, aggiungi due campi di primo livello
accanto a "artifacts" — "moment", "events" — giudicando l'INPUT come
una nota nel tempo:

- moment: {"intent":"remind"|"none","when":...,"rule":...}.
  "remind" solo quando la nota chiede al sé futuro di agire; tutto il resto è
  "none". "when" è la data e l'ora locali da orologio in ISO-8601 senza fuso
  (per es. 2026-09-04T09:00), oppure null se la nota non nomina alcun orario. Le
  parole relative (domani, venerdì prossimo, fra due settimane) si risolvono
  rispetto all'ora corrente che ti viene data. Quell'ora porta i minuti, e uno
  scarto inferiore a un giorno si conta da lì: alle 16:57, "fra 10 minuti" sono
  le 17:07 dello stesso giorno. Passa alla data successiva solo quando
  l'aritmetica supera davvero la mezzanotte. Un'ora del giorno non indicata è
  09:00, ma uno scarto ne indica una. "rule" è una RRULE iCalendar che usa solo
  FREQ, INTERVAL, BYDAY (codici dei giorni della settimana), BYMONTHDAY, UNTIL,
  COUNT quando la nota dice che si ripete, altrimenti null.
  Un giorno che la nota nomina una volta — oggi, stasera, domani, un giorno
  della settimana, una data — va in "when" e non è mai una regola: "venerdì" è
  null, "ogni venerdì" è FREQ=WEEKLY;BYDAY=FR. Non inventare mai una data.
- events: date che la nota indica e che non sono il promemoria stesso, come
  date-ora locali ISO-8601 ("il rilascio è il 12" → la sua data). Vuoto quando
  non ne indica nessuna.

Il giudizio non è un artefatto. Non scrivere mai un artefatto che descriva
l'intento della nota o ne ripeta le date: questo spetta solo a "moment" e "events". Una nota di una o
due frasi produce esattamente un artefatto, nella lingua della nota.

Senza blocco JUDGE, rispondi con "artifacts" da solo."#;

/// `SYNTHESIZER_SYSTEM` in NL. See `infer::lang`.
pub const SYNTHESIZER_SYSTEM_NL: &str = r#"Je maakt van vastgelegd materiaal atomaire, op zichzelf staande kennisartefacten, geschreven om door een semantische zoekopdracht teruggevonden te worden.

Elk artefact bevat precies één ding: één techniek, één procedure, één feit, één
beslissing, één configuratie. Behandelt een passage drie technieken, geef dan
drie artefacten.

Schrijf elk artefact als het zoekresultaat dat iemand over maanden zal lezen:
het staat op zichzelf, zonder het omringende document, begint met de termen
waar een mens op zou zoeken, en zegt zijn punt in de eerste zin. Los voornaam-
woorden en impliciete verwijzingen op: "dit commando" wordt het commando zelf,
"de map hierboven" wordt het pad zelf. Ongestructureerde notities komen
gestructureerd naar buiten — een telegramachtig fragment wordt een volledige
uitspraak.

Geef commando's, bestandspaden, registersleutels, foutmeldingen, code en
versienummers LETTERLIJK weer. Parafraseer, herformatteer, corrigeer of kort ze
nooit in. Het herschrijven geldt voor het verbindende proza eromheen, nooit voor
de letterlijke tekst zelf.

Een blok gemarkeerd met "context only" staat er zodat je verwijzingen kunt
oplossen — waar een voornaamwoord naar wijst, over welke versie of welk platform
het document gaat. Gebruik het om artefacten te schrijven die op zichzelf staan.
Geef nooit een artefact voor materiaal dat alleen in een contextblok voorkomt:
het venster waar dat materiaal bij hoort geeft het uit, en twee keer uitgeven
zet twee kopieën in de kennisbank. Een NEIGHBORS-blok toont wat de kennisbank al
bevat: schrijf wat de invoer toevoegt en herhaal nooit de inhoud van een buur.
Extraheer uitsluitend uit het INPUT-blok.

Schrijf de artefacttekst als markdown: afgebakende codeblokken met een taallabel,
lijsten voor stapsgewijze procedures, tabellen waar ze passen. Gebruik GEEN
H1-kop (`# `); de titel is een apart veld, dus koppen binnen de tekst beginnen
bij `## `.
Schrijf formules in LaTeX: `$…$` in de lopende tekst, `$$…$$` voor een formule
op een eigen regel; nooit `\(…\)` of `\[…\]`.

Antwoord uitsluitend met JSON, zonder commentaar, in precies deze vorm:

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title: een korte naamwoordgroep die het artefact benoemt.
- corpus_lines: het 1-gebaseerde regelbereik in de invoer waar dit artefact
  vandaan komt.
- caveats: 0-3 korte zinnen voor voorwaarden waaronder dit artefact niet geldt —
  een vereiste, een versie of platform waarvoor het specifiek is, een
  destructief effect, een gedocumenteerde fout. Neem ze alleen uit wat de invoer
  stelt of duidelijk impliceert. Verzin nooit een voorbehoud, voeg nooit
  algemeen advies toe, en zet nooit een commando in een voorbehoud dat niet in
  de invoer staat. Gebruik een lege lijst als de invoer er geen noemt, wat het
  gebruikelijke geval is.
- tags: 0-3 korte onderwerpswoorden in kleine letters. Leeg als geen
  onderwerpswoord voor de hand ligt.

Draagt de prompt een JUDGE-blok, voeg dan naast "artifacts" twee velden op het
hoogste niveau toe — "moment", "events" — en beoordeel de INPUT als een
notitie in de tijd:

- moment: {"intent":"remind"|"none","when":...,"rule":...}.
  "remind" alleen als de notitie het toekomstige zelf vraagt te handelen;
  al het andere is "none". "when" is de lokale klokdatum en -tijd als ISO-8601 zonder
  zone (bijv. 2026-09-04T09:00), of null als de notitie geen tijd noemt.
  Relatieve woorden (morgen, volgende vrijdag, over twee weken) worden opgelost
  tegen de huidige tijd die je krijgt. Die tijd draagt minuten, en een verschil
  korter dan een dag wordt daarvanaf geteld: om 16:57 is "over 10 minuten" 17:07
  op dezelfde dag. Ga alleen naar de volgende datum als de rekensom werkelijk
  middernacht passeert. Een niet genoemd tijdstip is 09:00, maar een verschil
  noemt er wel een. "rule" is een iCalendar-RRULE die alleen FREQ, INTERVAL,
  BYDAY (weekdagcodes), BYMONTHDAY, UNTIL, COUNT gebruikt als de notitie zegt
  dat het zich herhaalt, anders null.
  Een dag die de notitie één keer noemt — vandaag, vanavond, morgen, een
  weekdag, een datum — hoort in "when" en is nooit een regel: "op vrijdag" is
  null, "elke vrijdag" is FREQ=WEEKLY;BYDAY=FR. Verzin nooit een datum.
- events: data die de notitie noemt en die niet de herinnering zelf zijn, als
  lokale ISO-8601-datumtijden ("de release is op de 12e" → die datum). Leeg als
  ze er geen noemt.

Het oordeel is geen artefact. Schrijf nooit een artefact dat de bedoeling van
de notitie beschrijft of haar data herhaalt:
dat hoort alleen in "moment" en "events". Een notitie van één of
twee zinnen levert precies één artefact op, in de taal van de notitie.

Zonder JUDGE-blok antwoord je alleen met "artifacts"."#;

/// `SYNTHESIZER_SYSTEM` in PL. See `infer::lang`.
pub const SYNTHESIZER_SYSTEM_PL: &str = r#"Zamieniasz przechwycony materiał w atomowe, samodzielne artefakty wiedzy, pisane tak, by dało się je odnaleźć wyszukiwaniem semantycznym.

Każdy artefakt zawiera dokładnie jedną rzecz: jedną technikę, jedną procedurę,
jeden fakt, jedną decyzję, jedną konfigurację. Jeśli fragment obejmuje trzy
techniki, wypisz trzy artefakty.

Pisz każdy artefakt jak wynik wyszukiwania, który ktoś przeczyta za kilka
miesięcy: broni się sam, bez otaczającego dokumentu, zaczyna od słów, których
człowiek by szukał, i mówi swoje w pierwszym zdaniu. Rozwiązuj zaimki i
odwołania domyślne: „to polecenie" staje się samym poleceniem, „powyższy
katalog" staje się samą ścieżką. Notatki bez struktury wychodzą uporządkowane —
telegraficzny strzęp staje się pełnym zdaniem.

Polecenia, ścieżki plików, klucze rejestru, komunikaty błędów, kod i numery
wersji odtwarzaj DOSŁOWNIE. Nigdy ich nie parafrazuj, nie przeformatowuj, nie
poprawiaj ani nie skracaj. Przepisywanie dotyczy prozy, która je łączy, nigdy
samych literałów.

Blok oznaczony „context only" jest po to, byś mógł rozwiązać odwołania — na co
wskazuje zaimek, o której wersji lub platformie mówi dokument. Użyj go, by pisać
artefakty, które bronią się same. Nigdy nie wypisuj artefaktu dla materiału,
który pojawia się wyłącznie w bloku kontekstu: wypisze go okno, do którego ten
materiał należy, a wypisany dwa razy zostawia dwie kopie w bazie wiedzy. Blok
NEIGHBORS pokazuje, co baza już zawiera: pisz to, co wnosi wejście, i nigdy nie
powtarzaj treści sąsiada. Wydobywaj wyłącznie z bloku INPUT.

Tekst artefaktu pisz w markdownie: ogrodzone bloki kodu ze znacznikiem języka,
listy dla procedur krok po kroku, tabele tam, gdzie pasują. NIE używaj nagłówka
H1 (`# `); tytuł jest osobnym polem, więc nagłówki wewnątrz tekstu zaczynają się
od `## `.
Wzory pisz w LaTeX-u: `$…$` w tekście, `$$…$$` dla wzoru w osobnej linii; nigdy
`\(…\)` ani `\[…\]`.

Odpowiadaj wyłącznie JSON-em, bez komentarza, dokładnie w tej postaci:

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title: krótka fraza rzeczownikowa nazywająca artefakt.
- corpus_lines: zakres wierszy wejścia (liczony od 1), z którego pochodzi ten
  artefakt.
- caveats: 0-3 krótkie zdania o warunkach, w których ten artefakt nie
  obowiązuje — wymaganie wstępne, wersja lub platforma, dla której jest
  swoisty, skutek niszczący, udokumentowana awaria. Bierz je wyłącznie z tego,
  co wejście stwierdza lub wyraźnie implikuje. Nigdy nie wymyślaj zastrzeżenia,
  nie dodawaj ogólnych porad i nigdy nie umieszczaj w zastrzeżeniu polecenia,
  którego nie ma w wejściu. Użyj pustej listy, gdy wejście żadnego nie podaje —
  to przypadek typowy.
- tags: 0-3 krótkie słowa tematyczne małymi literami. Puste, gdy żadne słowo
  tematyczne nie narzuca się samo.

Gdy prompt niesie blok JUDGE, dodaj obok „artifacts" dwa pola najwyższego
poziomu — „moment", „events" — oceniając INPUT jako notatkę w czasie:

- moment: {"intent":"remind"|"none","when":...,"rule":...}.
  „remind" tylko wtedy, gdy notatka prosi przyszłe ja o działanie; wszystko
  inne to „none". „when" to lokalna data i godzina zegarowa w ISO-8601 bez strefy (np.
  2026-09-04T09:00) albo null, jeśli notatka nie podaje godziny. Słowa względne
  (jutro, w przyszły piątek, za dwa tygodnie) rozwiązuje się względem podanego
  ci bieżącego czasu. Ten czas niesie minuty, a przesunięcie krótsze niż doba
  liczy się od niego: o 16:57 „za 10 minut" to 17:07 tego samego dnia. Przechodź
  na następną datę tylko wtedy, gdy rachunek naprawdę przekracza północ.
  Niepodana pora dnia to 09:00, ale przesunięcie ją podaje. „rule" to RRULE
  iCalendar używająca wyłącznie FREQ, INTERVAL, BYDAY (kody dni tygodnia),
  BYMONTHDAY, UNTIL, COUNT, gdy notatka mówi o powtarzaniu; w przeciwnym razie
  null.
  Dzień nazwany w notatce raz — dziś, dziś wieczorem, jutro, dzień tygodnia,
  data — trafia do "when" i nigdy nie jest regułą: „w piątek" to null,
  „w każdy piątek" to FREQ=WEEKLY;BYDAY=FR. Nigdy nie wymyślaj daty.
- events: daty podane w notatce, które nie są samym przypomnieniem, jako lokalne
  daty i godziny ISO-8601 („wydanie jest 12." → jego data). Puste, gdy notatka
  żadnej nie podaje.

Ocena nie jest artefaktem. Nigdy nie pisz artefaktu opisującego intencję
notatki ani powtarzającego jej daty: to
należy wyłącznie do "moment" i "events". Notatka z jednego lub dwóch
zdań daje dokładnie jeden artefakt, w języku notatki.

Bez bloku JUDGE odpowiadaj samym „artifacts"."#;

/// `SYNTHESIZER_SYSTEM` in PT. See `infer::lang`.
pub const SYNTHESIZER_SYSTEM_PT: &str = r#"Transformas material capturado em artefactos de conhecimento atómicos e autossuficientes, escritos para voltarem a ser encontrados por uma pesquisa semântica.

Cada artefacto contém exatamente uma coisa: uma técnica, um procedimento, um
facto, uma decisão, uma configuração. Se uma passagem cobrir três técnicas,
produz três artefactos.

Escreve cada artefacto como o resultado de pesquisa que alguém vai ler daqui a
meses: sustenta-se sozinho, sem o documento à volta, abre com os termos que uma
pessoa procuraria e diz o que tem a dizer na primeira frase. Resolve pronomes e
referências implícitas: "este comando" passa a ser o comando em si, "o diretório
acima" passa a ser o caminho em si. Notas sem estrutura saem estruturadas — um
fragmento telegráfico torna-se uma afirmação completa.

Reproduz comandos, caminhos de ficheiro, chaves de registo, mensagens de erro,
código e números de versão LITERALMENTE. Nunca os parafraseies, reformates,
corrijas ou abrevies. A reescrita aplica-se à prosa que os liga, nunca aos
literais em si.

Um bloco marcado com "context only" está ali para que possas resolver
referências — para o que aponta um pronome, de que versão ou plataforma trata o
documento. Usa-o para escrever artefactos que se sustentem sozinhos. Nunca
produzas um artefacto para material que apareça apenas num bloco de contexto: a
janela a que esse material pertence produzi-lo-á, e produzi-lo duas vezes deixa
duas cópias na base de conhecimento. Um bloco NEIGHBORS mostra o que a base já
contém: escreve o que a entrada acrescenta e nunca repitas o conteúdo de um
vizinho. Extrai exclusivamente do bloco INPUT.

Escreve o texto do artefacto em markdown: blocos de código delimitados com
etiqueta de linguagem, listas para procedimentos passo a passo, tabelas onde
encaixem. NÃO uses um cabeçalho H1 (`# `); o título é um campo à parte, por isso
os cabeçalhos dentro do texto começam em `## `.
Escreve as fórmulas em LaTeX: `$…$` no texto, `$$…$$` para uma fórmula numa
linha própria; nunca `\(…\)` nem `\[…\]`.

Responde apenas com JSON, sem comentários, exatamente nesta forma:

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title: um sintagma nominal curto que nomeie o artefacto.
- corpus_lines: o intervalo de linhas (base 1) da entrada de onde veio este
  artefacto.
- caveats: 0-3 frases curtas com condições em que este artefacto não se
  verifica — um pré-requisito, uma versão ou plataforma a que é específico, um
  efeito destrutivo, uma falha documentada. Tira-as apenas do que a entrada
  afirma ou implica claramente. Nunca inventes uma ressalva, nunca acrescentes
  conselhos gerais e nunca ponhas numa ressalva um comando que não esteja na
  entrada. Usa uma lista vazia quando a entrada não indicar nenhuma, que é o
  caso comum.
- tags: 0-3 palavras temáticas curtas em minúsculas. Vazio quando nenhuma
  palavra temática for evidente.

Quando o prompt trouxer um bloco JUDGE, acrescenta dois campos de topo ao lado
de "artifacts" — "moment", "events" — julgando o INPUT como uma nota no
tempo:

- moment: {"intent":"remind"|"none","when":...,"rule":...}.
  "remind" só quando a nota pede ao eu futuro que aja; tudo o resto é
  "none". "when" é a
  data e hora locais de relógio em ISO-8601 sem fuso (p. ex.
  2026-09-04T09:00), ou null se a nota não indicar hora. Palavras relativas
  (amanhã, na próxima sexta, daqui a duas semanas) resolvem-se contra a hora
  atual que te é dada. Essa hora traz minutos, e um desvio inferior a um dia
  conta-se a partir dela: às 16:57, "daqui a 10 minutos" são 17:07 do mesmo
  dia. Só passa para a data seguinte quando a aritmética atravessar de facto a
  meia-noite. Uma hora do dia não indicada são 09:00, mas um desvio indica uma.
  "rule" é uma RRULE de iCalendar usando apenas FREQ, INTERVAL, BYDAY (códigos
  de dia da semana), BYMONTHDAY, UNTIL, COUNT quando a nota diz que se repete;
  caso contrário null.
  Um dia que a nota nomeia uma vez — hoje, esta noite, amanhã, um dia da
  semana, uma data — vai em "when" e nunca é uma regra: "na sexta" é null,
  "todas as sextas" é FREQ=WEEKLY;BYDAY=FR. Nunca inventes uma data.
- events: datas que a nota indica e que não são o próprio lembrete, como
  datas-horas locais ISO-8601 ("o lançamento é no dia 12" → a sua data). Vazio
  quando não indica nenhuma.

O julgamento não é um artefato. Nunca escreva um artefato que descreva a
intenção da nota ou repita as suas datas:
isso pertence apenas a "moment" e "events". Uma nota de uma ou duas
frases produz exatamente um artefato, na língua da nota.

Sem bloco JUDGE, responde apenas com "artifacts"."#;

/// `SYNTHESIZER_SYSTEM` in RU. See `infer::lang`.
pub const SYNTHESIZER_SYSTEM_RU: &str = r#"Ты превращаешь захваченный материал в атомарные, самодостаточные артефакты знания, написанные так, чтобы их снова нашёл семантический поиск.

Каждый артефакт содержит ровно одну вещь: один приём, одну процедуру, один
факт, одно решение, одну конфигурацию. Если фрагмент охватывает три приёма,
выдай три артефакта.

Пиши каждый артефакт как результат поиска, который кто-то прочитает через
несколько месяцев: он стоит сам по себе, без окружающего документа, начинается
со слов, которые человек стал бы искать, и излагает суть в первом предложении.
Раскрывай местоимения и неявные отсылки: «эта команда» становится самой
командой, «каталог выше» становится самим путём. Неструктурированные заметки
выходят структурированными — телеграфный обрывок становится законченным
утверждением.

Команды, пути к файлам, ключи реестра, строки ошибок, код и номера версий
воспроизводи ДОСЛОВНО. Никогда не перефразируй, не переформатируй, не исправляй
и не сокращай их. Переписывание касается связующей прозы вокруг них, но никогда
самих литералов.

Блок, помеченный "context only", нужен, чтобы ты мог раскрыть отсылки — на что
указывает местоимение, о какой версии или платформе идёт речь в документе.
Используй его, чтобы писать артефакты, стоящие сами по себе. Никогда не выдавай
артефакт по материалу, который встречается только в контекстном блоке: его
выдаст то окно, которому этот материал принадлежит, а выданный дважды он
оставит две копии в базе знаний. Блок NEIGHBORS показывает, что база уже
содержит: пиши то, что добавляет вход, и никогда не пересказывай содержание
соседа. Извлекай исключительно из блока INPUT.

Пиши текст артефакта в markdown: огороженные блоки кода с указанием языка,
списки для пошаговых процедур, таблицы там, где они уместны. НЕ используй
заголовок H1 (`# `); заголовок — отдельное поле, поэтому заголовки внутри
текста начинаются с `## `.
Формулы пиши в LaTeX: `$…$` внутри текста, `$$…$$` для формулы на отдельной
строке; никогда не используй `\(…\)` или `\[…\]`.

Отвечай только JSON, без комментариев, ровно в такой форме:

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title: короткая именная группа, называющая артефакт.
- corpus_lines: диапазон строк входа (нумерация с 1), из которого взят этот
  артефакт.
- caveats: 0-3 коротких предложения об условиях, при которых этот артефакт не
  выполняется, — предварительное требование, версия или платформа, к которой он
  привязан, разрушительный эффект, задокументированный отказ. Бери их только из
  того, что вход утверждает или явно подразумевает. Никогда не выдумывай
  оговорку, не добавляй общих советов и никогда не помещай в оговорку команду,
  которой нет во входе. Используй пустой список, когда вход не называет ни
  одной, — это обычный случай.
- tags: 0-3 коротких тематических слова строчными буквами. Пусто, когда ни одно
  тематическое слово не напрашивается.

Когда в промпте есть блок JUDGE, добавь рядом с "artifacts" два поля верхнего
уровня — "moment", "events" — оценивая INPUT как заметку во времени:

- moment: {"intent":"remind"|"none","when":...,"rule":...}.
  "remind" только когда заметка просит будущего себя действовать; всё
  остальное — "none". "when" — местные дата и время по часам в ISO-8601 без зоны
  (например, 2026-09-04T09:00), или null, если заметка не называет времени.
  Относительные слова (завтра, в следующую пятницу, через две недели)
  раскрываются относительно данного тебе текущего времени. В этом времени есть
  минуты, и сдвиг меньше суток отсчитывается от него: в 16:57 «через 10 минут»
  — это 17:07 того же дня. Переходи на следующую дату только тогда, когда
  арифметика действительно перешагивает полночь. Не названное время суток —
  09:00, но сдвиг его называет. "rule" — это iCalendar RRULE, использующая
  только FREQ, INTERVAL, BYDAY (коды дней недели), BYMONTHDAY, UNTIL, COUNT,
  когда заметка говорит о повторении, иначе null.
  День, названный в заметке один раз, — сегодня, сегодня вечером, завтра, день
  недели, дата — идёт в "when" и никогда не бывает правилом: «в пятницу» —
  null, «каждую пятницу» — FREQ=WEEKLY;BYDAY=FR. Никогда не выдумывай дату.
- events: даты, названные в заметке и не являющиеся самим напоминанием, как
  местные ISO-8601 дата-время («релиз 12-го» → его дата). Пусто, когда она не
  называет ни одной.

Суждение — не артефакт. Никогда не пиши артефакт, который описывает намерение
заметки или повторяет её даты: это место только
для "moment" и "events". Заметка из одного-двух предложений даёт
ровно один артефакт, на языке заметки.

Без блока JUDGE отвечай одним лишь "artifacts"."#;

/// `SYNTHESIZER_SYSTEM` in TR. See `infer::lang`.
pub const SYNTHESIZER_SYSTEM_TR: &str = r#"Yakalanan malzemeyi, anlamsal aramayla yeniden bulunmak üzere yazılmış, atomik ve kendi başına duran bilgi artefaktlarına dönüştürürsün.

Her artefakt tam olarak tek bir şey taşır: bir teknik, bir yordam, bir olgu, bir
karar, bir yapılandırma. Bir pasaj üç tekniği kapsıyorsa üç artefakt üret.

Her artefaktı, birinin aylar sonra okuyacağı arama sonucu gibi yaz: çevresindeki
belge olmadan kendi başına durur, bir insanın arayacağı terimlerle açılır ve
söyleyeceğini ilk cümlede söyler. Adılları ve örtük göndermeleri çöz: "bu
komut", komutun kendisi olur; "yukarıdaki dizin", yolun kendisi olur.
Yapılandırılmamış notlar yapılandırılmış çıkar — telgraf gibi bir parça, tam bir
ifadeye dönüşür.

Komutları, dosya yollarını, kayıt defteri anahtarlarını, hata metinlerini, kodu
ve sürüm numaralarını BİREBİR aktar. Onları asla başka sözcüklerle anlatma,
yeniden biçimlendirme, düzeltme veya kısaltma. Yeniden yazma, onları birbirine
bağlayan düzyazı içindir; hiçbir zaman değişmezlerin kendisi için değil.

"context only" ile işaretlenmiş bir blok, göndermeleri çözebilmen için oradadır
— bir adılın neyi gösterdiği, belgenin hangi sürüm ya da platformla ilgili
olduğu. Kendi başına duran artefaktlar yazmak için onu kullan. Yalnızca bir
bağlam bloğunda geçen malzeme için asla artefakt üretme: o malzemenin ait olduğu
pencere onu üretecektir ve iki kez üretmek bilgi tabanına iki kopya koyar. Bir
NEIGHBORS bloğu, bilgi tabanının halihazırda tuttuklarını gösterir: girdinin
eklediğini yaz, bir komşunun içeriğini asla yeniden anlatma. Yalnızca INPUT
bloğundan çıkar.

Artefakt metnini markdown olarak yaz: dil etiketli çitli kod blokları, adım adım
yordamlar için listeler, uyduğu yerde tablolar. H1 başlığı (`# `) KULLANMA;
başlık ayrı bir alandır, dolayısıyla metnin içindeki başlıklar `## ` ile başlar.
Formülleri LaTeX ile yaz: metin içinde `$…$`, kendi satırındaki bir formül için
`$$…$$`; asla `\(…\)` veya `\[…\]` kullanma.

Yalnızca JSON ile, yorumsuz, tam olarak şu biçimde yanıtla:

{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}

- title: artefaktı adlandıran kısa bir ad öbeği.
- corpus_lines: bu artefaktın geldiği, girdideki 1 tabanlı satır aralığı.
- caveats: bu artefaktın geçerli olmadığı koşullar için 0-3 kısa cümle — bir ön
  koşul, özgü olduğu bir sürüm ya da platform, yıkıcı bir etki, belgelenmiş bir
  başarısızlık. Bunları yalnızca girdinin belirttiğinden veya açıkça ima
  ettiğinden al. Asla bir çekince uydurma, asla genel öğüt ekleme ve bir
  çekinceye girdide bulunmayan bir komutu asla koyma. Girdi hiçbirini
  belirtmiyorsa boş liste kullan; olağan durum budur.
- tags: 0-3 kısa, küçük harfli konu sözcüğü. Belirgin bir konu sözcüğü yoksa
  boş.

Prompt bir JUDGE bloğu taşıyorsa, "artifacts" yanına iki üst düzey alan ekle —
"moment", "events" — ve INPUT'u zaman içindeki bir not olarak
değerlendir:

- moment: {"intent":"remind"|"none","when":...,"rule":...}.
  "remind" yalnızca not gelecekteki benlikten eyleme geçmesini istiyorsa;
  geri kalan her şey "none". "when", bölgesiz ISO-8601 biçiminde yerel duvar saati tarihi
  ve saatidir (ör. 2026-09-04T09:00) ya da not bir zaman söylemiyorsa null.
  Göreli sözcükler (yarın, gelecek cuma, iki hafta sonra) sana verilen şimdiki
  zamana göre çözülür. O zaman dakikaları taşır ve bir günden kısa bir kayma
  ondan sayılır: 16:57'de "10 dakika sonra", aynı günün 17:07'sidir. Yalnızca
  aritmetik gerçekten gece yarısını geçtiğinde bir sonraki tarihe geç.
  Söylenmemiş bir günün saati 09:00'dur, ama bir kayma saati söyler. "rule",
  not yinelendiğini söylüyorsa yalnızca FREQ, INTERVAL, BYDAY (hafta günü
  kodları), BYMONTHDAY, UNTIL, COUNT kullanan bir iCalendar RRULE'üdür; aksi
  hâlde null.
  Notun bir kez andığı bir gün — bugün, bu akşam, yarın, bir hafta günü, bir
  tarih — "when" alanına girer ve asla bir kural değildir: "cuma günü"
  null'dur, "her cuma" FREQ=WEEKLY;BYDAY=FR'dir. Asla bir tarih uydurma.
- events: notun belirttiği ve anımsatıcının kendisi olmayan tarihler, yerel
  ISO-8601 tarih-saat olarak ("sürüm ayın 12'sinde" → o tarih). Hiçbirini
  belirtmiyorsa boş.

Yargı bir artefakt değildir. Notun niyetini anlatan ya da tarihlerini yineleyen
bir artefakt asla yazma: bunlar yalnızca
"moment" ve "events" içindir. Bir ya da iki cümlelik bir not, notun
dilinde tam olarak bir artefakt verir.

JUDGE bloğu yoksa yalnızca "artifacts" ile yanıtla."#;

/// `TITLE_SYSTEM` in DE.
pub const TITLE_SYSTEM_DE: &str = r#"Du benennst Dokumente. Gegeben den Anfang eines Dokuments und die Titel der daraus gemachten Notizen, antworte mit einem kurzen Titel — höchstens acht Wörter, keine Anführungszeichen, kein Satzzeichen am Ende, keine Vorrede.

Benenne, worum es im Dokument geht, nicht was es ist. Niemals "Dokument", "Notizen", "Anleitung", "Ohne Titel"."#;

/// `TITLE_SYSTEM` in ES.
pub const TITLE_SYSTEM_ES: &str = r#"Nombras documentos. Dado el comienzo de un documento y los títulos de las notas tomadas de él, responde con un solo título corto: como máximo ocho palabras, sin comillas, sin puntuación final, sin preámbulo.

Nombra de qué trata el documento, no qué es. Nunca "Documento", "Notas", "Guía", "Sin título"."#;

/// `TITLE_SYSTEM` in FR.
pub const TITLE_SYSTEM_FR: &str = r#"Tu nommes des documents. Étant donné le début d'un document et les titres des notes qui en ont été tirées, réponds par un seul titre court : huit mots au maximum, sans guillemets, sans ponctuation finale, sans préambule.

Nomme ce dont parle le document, pas ce qu'il est. Jamais « Document », « Notes », « Guide », « Sans titre »."#;

/// `TITLE_SYSTEM` in IT.
pub const TITLE_SYSTEM_IT: &str = r#"Dai un nome ai documenti. Dato l'inizio di un documento e i titoli delle note che ne sono state tratte, rispondi con un solo titolo breve: al massimo otto parole, senza virgolette, senza punteggiatura finale, senza preamboli.

Nomina di cosa tratta il documento, non che cosa è. Mai "Documento", "Note", "Guida", "Senza titolo"."#;

/// `TITLE_SYSTEM` in NL.
pub const TITLE_SYSTEM_NL: &str = r#"Je geeft documenten een naam. Gegeven het begin van een document en de titels van de notities die eruit zijn gemaakt, antwoord met één korte titel — hoogstens acht woorden, geen aanhalingstekens, geen leesteken aan het eind, geen inleiding.

Benoem waar het document over gaat, niet wat het is. Nooit "Document", "Notities", "Handleiding", "Zonder titel"."#;

/// `TITLE_SYSTEM` in PL.
pub const TITLE_SYSTEM_PL: &str = r#"Nazywasz dokumenty. Mając początek dokumentu i tytuły sporządzonych z niego notatek, odpowiedz jednym krótkim tytułem — najwyżej osiem słów, bez cudzysłowów, bez znaku interpunkcyjnego na końcu, bez wstępu.

Nazwij, o czym jest dokument, a nie czym jest. Nigdy „Dokument", „Notatki", „Przewodnik", „Bez tytułu"."#;

/// `TITLE_SYSTEM` in PT.
pub const TITLE_SYSTEM_PT: &str = r#"Dás nome a documentos. Dado o início de um documento e os títulos das notas dele tiradas, responde com um único título curto — no máximo oito palavras, sem aspas, sem pontuação final, sem preâmbulo.

Nomeia o assunto do documento, não o que ele é. Nunca "Documento", "Notas", "Guia", "Sem título"."#;

/// `TITLE_SYSTEM` in RU.
pub const TITLE_SYSTEM_RU: &str = r#"Ты называешь документы. Получив начало документа и заголовки сделанных из него заметок, ответь одним коротким названием — не более восьми слов, без кавычек, без завершающего знака препинания, без предисловия.

Назови, о чём документ, а не что он такое. Никогда «Документ», «Заметки», «Руководство», «Без названия»."#;

/// `TITLE_SYSTEM` in TR.
pub const TITLE_SYSTEM_TR: &str = r#"Belgeleri adlandırırsın. Bir belgenin başlangıcı ve ondan çıkarılan notların başlıkları verildiğinde, tek bir kısa başlıkla yanıtla — en fazla sekiz sözcük, tırnak yok, sonda noktalama yok, giriş yok.

Belgenin neyle ilgili olduğunu adlandır, ne olduğunu değil. Asla "Belge", "Notlar", "Kılavuz", "Başlıksız"."#;

/// Translated, not "answer in German": a 9B model drifts off a language request but not off a language.
pub fn synthesizer_system(lang: crate::infer::lang::Lang) -> &'static str {
    use crate::infer::lang::Lang;
    match lang {
        Lang::En => SYNTHESIZER_SYSTEM,
        Lang::De => SYNTHESIZER_SYSTEM_DE,
        Lang::Es => SYNTHESIZER_SYSTEM_ES,
        Lang::Fr => SYNTHESIZER_SYSTEM_FR,
        Lang::It => SYNTHESIZER_SYSTEM_IT,
        Lang::Nl => SYNTHESIZER_SYSTEM_NL,
        Lang::Pl => SYNTHESIZER_SYSTEM_PL,
        Lang::Pt => SYNTHESIZER_SYSTEM_PT,
        Lang::Ru => SYNTHESIZER_SYSTEM_RU,
        Lang::Tr => SYNTHESIZER_SYSTEM_TR,
    }
}

pub fn title_system(lang: crate::infer::lang::Lang) -> &'static str {
    use crate::infer::lang::Lang;
    match lang {
        Lang::En => TITLE_SYSTEM,
        Lang::De => TITLE_SYSTEM_DE,
        Lang::Es => TITLE_SYSTEM_ES,
        Lang::Fr => TITLE_SYSTEM_FR,
        Lang::It => TITLE_SYSTEM_IT,
        Lang::Nl => TITLE_SYSTEM_NL,
        Lang::Pl => TITLE_SYSTEM_PL,
        Lang::Pt => TITLE_SYSTEM_PT,
        Lang::Ru => TITLE_SYSTEM_RU,
        Lang::Tr => TITLE_SYSTEM_TR,
    }
}

pub fn user_prompt(
    segment_text: &str,
    first_line: i64,
    max_artifact_tokens: usize,
    context: &crate::infer::context::WindowContext,
    judge: Option<&crate::infer::JudgeAsk>,
) -> String {
    let mut out = String::new();
    // The opening leads so system prompt plus opening is a byte-identical, cacheable prefix across windows.
    if let Some(o) = &context.opening {
        out.push_str(&format!(
            "----- DOCUMENT OPENING (context only) -----\n{o}\n\
             ----- END DOCUMENT OPENING -----\n\n"
        ));
    }
    if let Some(b) = &context.before {
        out.push_str(&format!(
            "----- PRECEDING CONTEXT (context only) -----\n{b}\n\
             ----- END PRECEDING CONTEXT -----\n\n"
        ));
    }
    out.push_str(&format!(
        "The input below starts at line {first_line}. Keep each artifact under roughly \
         {max_artifact_tokens} tokens; split into more artifacts rather than exceeding it.\n\n\
         ----- INPUT -----\n{segment_text}\n----- END INPUT -----"
    ));
    if let Some(a) = &context.after {
        out.push_str(&format!(
            "\n\n----- FOLLOWING CONTEXT (context only) -----\n{a}\n\
             ----- END FOLLOWING CONTEXT -----"
        ));
    }
    if let Some(j) = judge {
        if !j.neighbors.is_empty() {
            out.push_str("\n\n----- NEIGHBORS (context only) -----");
            for n in &j.neighbors {
                let title = n.title.as_deref().unwrap_or("untitled");
                out.push_str(&format!("\n[id: {}] {title}\n{}", n.id, n.text));
            }
            out.push_str("\n----- END NEIGHBORS -----");
        }
        out.push_str(&format!(
            "\n\n----- JUDGE -----\nCurrent local time: {}\nTime zone: {}\n",
            j.now_local, j.tz
        ));
        if let Some(f) = &j.forced_intent {
            out.push_str(&format!("The capture door says this is: {f}\n"));
        }
        out.push_str("Judge this note: moment, events.\n----- END JUDGE -----");
    }
    out
}

pub fn repair_prompt(previous: &str, err: &str) -> String {
    format!(
        "Your previous reply could not be parsed as JSON.\n\nParser error: {err}\n\n\
         Your reply was:\n{previous}\n\n\
         Reply again with valid JSON only, matching the required shape exactly. \
         No prose, no code fences."
    )
}

pub const TITLE_SYSTEM: &str = r#"You name documents. Given the opening of a document and the titles of the notes taken from it, reply with one short title — at most eight words, no quotes, no trailing punctuation, no preamble.

Name what the document is about, not what it is. Never "Document", "Notes", "Guide", "Untitled"."#;

/// The opening, not the whole document: the artifact titles already cover the rest.
pub fn title_prompt(text: &str, artifact_titles: &[String]) -> String {
    let opening: String = text.chars().take(2000).collect();
    format!(
        "Opening of the document:\n{opening}\n\nTitles of the notes taken from it:\n{}\n\nTitle:",
        artifact_titles.join("\n")
    )
}

#[derive(Debug, Clone)]
pub struct MergedDraft {
    pub title: Option<String>,
    pub text: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub caveats: Vec<String>,
}

pub const RESCUE_SYSTEM: &str = r#"You rewrite the still-valuable part of a retired knowledge-base artifact into one self-contained live artifact.

You are given source excerpts, the closest live artifacts, and the one line naming what the sources state that the live base does not. Write an artifact carrying exactly that: use only the excerpts that bear on it and leave the rest out. Write only what the excerpts support — every command, path, version, port and flag in your text must appear in an excerpt verbatim. Atomic — one subject, standing alone, readable without the excerpts. Do not restate what the live artifacts shown already say. Write formulas in LaTeX: `$…$` inline, `$$…$$` on a line of their own; never `\(…\)` or `\[…\]`.

Reply with JSON only: {"artifact":{"title":"…","text":"…","category":"…","tags":[],"caveats":[]}}"#;

pub const CONDENSE_SYSTEM: &str = r#"You rewrite one knowledge-base artifact shorter, keeping everything it states.

You are given the artifact and the wording of questions that reached it. Shorten the prose only: cut repetition, filler and restatement, and join sentences that say one thing twice. Keep everything the artifact states — every claim, condition and qualification, and every value, command, path, version, port, flag, date and error string exactly as written. Keep the title unless it is wrong, and keep every caveat.

The questions show you what this artifact is asked for. They do not tell you what to drop. A detail no question happened to mention is the reason the next question will find this artifact rather than a similar one, so it stays. If the artifact cannot be made shorter without dropping something it states, reply with it unchanged.

Do not add anything. Do not restate the questions.

Reply with JSON only: {"artifact":{"title":"…","text":"…","category":"…","tags":[],"caveats":[]}}"#;

/// A person already decided that these cover the same ground; this only writes the merge.
/// The merge rules are restated because a merge that drops a number is worse than none.
pub const SYNTHESIZE_SYSTEM: &str = r#"You are given two knowledge artifacts that a person has already judged to cover the same ground. Whether they do is settled and is not your question. Write one artifact that says everything both of them said.

The merged text must contain every number, version, date, path, flag, command and error string that appeared in either input. Where two of them disagree, keep both values and say what distinguishes them — the version, the platform, the date, whichever the inputs give. Dropping one is the failure this task exists to avoid.

Name the distinguishing condition, never the input it came from. "8080 on the old build, 9090 since 1.4" is right; "8080 in the other document" is not, because a reader who never sees the originals cannot resolve it. For the same reason the text must read as one self-contained artifact rather than a list of sources, with no "as above" and no reference to a document the reader does not have. Write formulas in LaTeX: `$…$` inline, `$$…$$` on a line of their own; never `\(…\)` or `\[…\]`.

The title names the subject. An artifact whose body never says what it is about is what makes it unfindable later — a section headed "FAT32" becomes a body that opens "32 Bit Clusternummern" and never says FAT32 again, and then only the title can answer what it is for.

Reply with JSON only, no commentary, in exactly this shape:

{"merged": {"title": "...", "text": "...", "category": "...", "caveats": []}}

- text: the merged artifact. Never empty.
- title: what it is about.
- category: one of the listed categories.
- caveats: the conditions under which it does not apply; an empty list when there are none."#;

/// Every property required: under `strict` the hosted APIs reject a listed-but-optional property.
pub fn synthesize_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "merged": {
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "title": {"type": "string"},
                    "category": {"type": "string", "enum": CATEGORIES},
                    "caveats": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["text", "title", "category", "caveats"],
                "additionalProperties": false
            }
        },
        "required": ["merged"],
        "additionalProperties": false
    })
}

/// No salvage: the judgement was a person's, so a reply without usable text is a failed call.
pub fn parse_synthesis(body: &str) -> Result<MergedDraft> {
    #[derive(serde::Deserialize)]
    struct Raw {
        merged: RawMerged,
    }
    #[derive(serde::Deserialize)]
    struct RawMerged {
        text: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        category: Option<String>,
        #[serde(default)]
        caveats: Vec<String>,
    }
    let raw: Raw = serde_json::from_str(extract_json(body)).map_err(|e| {
        Error::MalformedLlmOutput(format!("synthesis reply was not the expected JSON: {e}"))
    })?;
    if raw.merged.text.trim().is_empty() {
        return Err(Error::MalformedLlmOutput(
            "synthesis reply carried no text".into(),
        ));
    }
    Ok(MergedDraft {
        title: raw.merged.title.map(|t| t.trim().to_string()),
        text: raw.merged.text,
        category: raw.merged.category.as_deref().map(normalize_category),
        tags: Vec::new(),
        caveats: raw.merged.caveats,
    })
}

/// One definition for what the model is told and what `abstained` looks for.
pub const ABSTAIN_PREFIX: &str = "Not in the knowledge base";

/// Abstention is a lexical test (does any excerpt mention any part of the subject): every weighed wording made a 9B
/// model loop, 25 to 113 restarts measured. [`ABSTAIN_PREFIX`] must reach the reply verbatim for [`abstained`].
pub const ASK_SYSTEM: &str = "You answer questions using only the provided knowledge-base excerpts. \
Quote commands, paths and code exactly as they appear, and cite each claim by the number of the \
excerpt it came from. \
Write formulas in LaTeX: `$…$` inline, `$$…$$` on a line of their own; never `\\(…\\)` or \
`\\[…\\]`. \
Answer whatever the excerpts cover and say plainly what they do not, without stretching an excerpt \
to cover what it does not. \
Abstain only when no excerpt mentions any part of the subject: then begin your reply with the exact \
words `Not in the knowledge base.` and say what is missing rather than guessing. \
A subject the excerpts carry in part is answered in part and never abstained on. \
`(continues [n])` marks an excerpt whose text is printed under excerpt n; cite whichever of the two \
numbers holds the words you used. \
Lines beginning `Caveat:` give the conditions under which an excerpt does not apply — repeat any \
that bears on your answer. \
Where a caveat says another note states something differently and your answer rests on it, give both readings with their dates and cite each one that is excerpted; do not choose between them.";

/// Lead-in marks and quotes are skipped case-insensitively; a quote that closes right after the phrase with text
/// behind it is a quotation, not an abstention. Compared by characters: non-ASCII lead-ins break byte slicing.
pub fn abstained(answer: &str) -> bool {
    let quote = |c: char| matches!(c, '"' | '\'' | '„' | '“' | '”' | '‘' | '’' | '«' | '»');
    let marks = |c: char| {
        c.is_whitespace()
            || quote(c)
            || matches!(c, '*' | '_' | '#' | '>' | '`' | '-' | '+' | '•' | '·')
    };
    let opening = answer.trim_start_matches(marks);
    // An ordered list marker is the one lead-in that is not a single character class.
    let digits = opening.len()
        - opening
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    let opening = match opening[digits..].strip_prefix(['.', ')']) {
        Some(rest) if digits > 0 => rest.trim_start_matches(marks),
        _ => opening,
    };
    let head: String = opening
        .chars()
        .take(ABSTAIN_PREFIX.chars().count())
        .collect();
    if !head.eq_ignore_ascii_case(ABSTAIN_PREFIX) {
        return false;
    }
    if !answer[..answer.len() - opening.len()].contains(quote) {
        return true;
    }
    // Closing right after the phrase with an answer behind it means the phrase was being quoted.
    let rest = &opening[head.len()..];
    match rest.chars().next() {
        Some(c) if quote(c) => rest[c.len_utf8()..].trim().is_empty(),
        _ => true,
    }
}

/// Caveats are appended here because their `Caveat:` prefix is what `ASK_SYSTEM` tells the model to look for.
/// `dated` only for excerpts in a disagreement, which the model must give with both dates.
pub fn ask_excerpt(
    number: usize,
    title: &str,
    dated: Option<&str>,
    text: &str,
    caveats: &[String],
) -> String {
    let mut block = match dated {
        Some(day) => format!("[{number}] {title} ({day})\n{text}"),
        None => format!("[{number}] {title}\n{text}"),
    };
    for c in caveats {
        block.push_str("\nCaveat: ");
        block.push_str(c);
    }
    block
}

/// Keeps its own number: the rail links each number to an artifact the model may cite.
pub fn ask_continues(number: usize, printed_under: usize) -> String {
    format!("[{number}] (continues [{printed_under}])")
}

pub fn ask_prompt(question: &str, excerpts: &[String]) -> String {
    // A bare `[7]` in the prompt is worse than one number unused.
    let shown: Vec<&str> = excerpts
        .iter()
        .map(String::as_str)
        .filter(|e| !e.is_empty())
        .collect();
    format!(
        "Question: {question}\n\nExcerpts:\n\n{}",
        shown.join("\n\n---\n\n")
    )
}

/// Also the fan-out's concurrency bound, so one number governs both.
pub const PLAN_MAX_QUERIES: usize = 3;

/// Asked once over the whole question, never to answer or judge itself: bounded, not an agent.
pub const PLAN_SYSTEM: &str = r#"You are helping a search system decide what else to look for.

You are given a question and the excerpts retrieved for it so far. A question
can span several subjects; the excerpts may cover some of them and miss others.

Reply with JSON only, in exactly this shape:

{"need": ["a short search query", "another one"]}   or   {"need": []}

- An empty list: the excerpts already cover every subject the question raises.
  This is the common answer.
- Otherwise one query per subject the excerpts do NOT yet cover — at most three,
  fewest first. Each is the words you would type into a search box: not a
  question, not a sentence, and never the original question repeated back.
  Do not name a subject the excerpts already cover just to be thorough."#;

/// No `maxItems`: some grammar compilers reject the whole request over it; `parse_plan` truncates instead.
pub fn plan_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "need": {
                "type": "array",
                "items": {"type": "string"}
            }
        },
        "required": ["need"],
        "additionalProperties": false
    })
}

/// Every failure reads as empty; a bare string is the one query it plainly is. Blanks and repeats are dropped.
pub fn parse_plan(reply: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(extract_json(reply)) else {
        return Vec::new();
    };
    let raw: Vec<&str> = match &v["need"] {
        serde_json::Value::String(one) => vec![one.as_str()],
        serde_json::Value::Array(many) => many.iter().filter_map(|q| q.as_str()).collect(),
        _ => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for q in raw {
        let q = q.trim();
        if q.is_empty() || out.iter().any(|kept| kept.eq_ignore_ascii_case(q)) {
            continue;
        }
        out.push(q.to_string());
        if out.len() == PLAN_MAX_QUERIES {
            break;
        }
    }
    out
}

/// Sees questions only, never answers: it names the hole, not the guess.
pub const GAP_LABEL_SYSTEM: &str = r#"You name topics. Given several questions a knowledge base could not answer, reply with the name of the subject they share — three to six words, a noun phrase, no quotes, no trailing punctuation. Reply with JSON only: {"label":"…"}"#;

/// Bounded because an over-window prompt fails and the fallback label re-pays that failure every sweep.
pub const GAP_LABEL_MAX_QUESTIONS: usize = 12;
pub const GAP_LABEL_MAX_CHARS: usize = 200;

/// Questions arrive newest first, so the cap keeps the newest.
pub fn gap_label_prompt(questions: &[&str]) -> String {
    let mut s = String::from("Questions:\n");
    for q in questions.iter().take(GAP_LABEL_MAX_QUESTIONS) {
        s.push_str("- ");
        match q.char_indices().nth(GAP_LABEL_MAX_CHARS) {
            Some((cut, _)) => {
                s.push_str(&q[..cut]);
                s.push('…');
            }
            None => s.push_str(q),
        }
        s.push('\n');
    }
    s
}

/// Carries `ASK_SYSTEM`'s abstention rule: clicked excerpts may not answer, and a stored refusal would be retrieved later.
pub const GENERATE_SYSTEM: &str = r#"You write one self-contained knowledge-base artifact from the excerpts you are given, to answer the questions listed. Answer the questions and nothing else: use only the excerpts that bear on them and leave the rest out, however much they say. Write only what the excerpts support: every command, path, version, port and flag in your text must appear in an excerpt verbatim. Atomic — one subject, standing alone, readable without the excerpts. Abstain only when no excerpt mentions any part of the subject the questions ask about: then make the artifact text begin with the exact words `Not in the knowledge base.` and say what is missing. Never write an artifact whose subject is that the excerpts fall short. Write formulas in LaTeX: `$…$` inline, `$$…$$` on a line of their own; never `\(…\)` or `\[…\]`. Reply with JSON only: {"artifact":{"title":"…","text":"…","category":"…","tags":[],"caveats":[]}}"#;

pub fn generate_prompt(questions: &[String], sources: &[(String, String)]) -> String {
    let qs = questions
        .iter()
        .map(|q| format!("- {q}"))
        .collect::<Vec<_>>()
        .join("\n");
    let ex = sources
        .iter()
        .enumerate()
        .map(|(i, (title, text))| format!("[{}] {title}\n{text}", i + 1))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    format!("Questions:\n{qs}\n\nExcerpts:\n\n{ex}")
}

pub fn generate_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "artifact": {
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "text": {"type": "string"},
                    "category": {"type": "string", "enum": CATEGORIES},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "caveats": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["title", "text", "category", "tags", "caveats"],
                "additionalProperties": false
            }
        },
        "required": ["artifact"],
        "additionalProperties": false
    })
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Generated {
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub caveats: Vec<String>,
}

pub fn parse_generation(reply: &str) -> Result<Generated> {
    let v: serde_json::Value = serde_json::from_str(extract_json(reply))
        .map_err(|e| Error::MalformedLlmOutput(format!("generation was not JSON: {e}")))?;
    let g: Generated = serde_json::from_value(v["artifact"].clone())
        .map_err(|e| Error::MalformedLlmOutput(format!("generation had no artifact: {e}")))?;
    if g.text.trim().is_empty() {
        return Err(Error::MalformedLlmOutput("generation had no text".into()));
    }
    Ok(g)
}

pub fn gap_label_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "label": {"type": "string"} },
        "required": ["label"],
        "additionalProperties": false
    })
}

/// An empty label is an error: a cluster must be called something.
pub fn parse_gap_label(reply: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(extract_json(reply))
        .map_err(|e| Error::MalformedLlmOutput(format!("gap label was not JSON: {e}")))?;
    let label = v["label"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.')
        .trim()
        .to_string();
    if label.is_empty() {
        return Err(Error::MalformedLlmOutput("gap label was empty".into()));
    }
    Ok(label)
}

#[derive(serde::Deserialize)]
struct Envelope {
    artifacts: Vec<RawArtifact>,
}

/// A malformed judgement half must never cost the artifacts beside it.
fn lenient<'de, D, T>(d: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(<serde_json::Value as serde::Deserialize>::deserialize(d)
        .ok()
        .and_then(|v| serde_json::from_value(v).ok()))
}

#[derive(serde::Deserialize)]
struct JudgedEnvelope {
    /// Defaulted: without an enforced grammar a reminder-only reply omits the key, and `salvage_objects` cannot rescue it.
    #[serde(default)]
    artifacts: Vec<RawArtifact>,
    #[serde(default, deserialize_with = "lenient")]
    moment: Option<RawMoment>,
    #[serde(default, deserialize_with = "lenient")]
    events: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
struct RawMoment {
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    rule: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawArtifact {
    text: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    corpus_lines: Option<Vec<i64>>,
    #[serde(default)]
    caveats: Vec<String>,
    #[serde(default, deserialize_with = "lenient")]
    tags: Option<Vec<String>>,
    // An older reply's `category` and `pinned` are unknown fields, ignored whatever their shape.
}

/// Form, not subject, and closed: a free string filled the filter row with domains. Unknown words become `other`.
pub const CATEGORIES: &[&str] = &[
    "concept",
    "procedure",
    "reference",
    "snippet",
    "configuration",
    "definition",
    "example",
    "other",
];

/// Never rejects: a good artifact with an unrecognised label is still good.
pub fn normalize_category(raw: &str) -> String {
    let t = raw.trim().to_ascii_lowercase();
    if CATEGORIES.contains(&t.as_str()) {
        t
    } else {
        "other".to_string()
    }
}

/// Kept beside `RawArtifact`: a drifted schema constrains the model into output the parser rejects.
/// No category, pin or links: the decision model reads and links what was written.
/// Every field required, though serde keeps them optional for older replies.
pub fn artifacts_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "artifacts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "title": {"type": "string"},
                        "corpus_lines": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "minItems": 2,
                            "maxItems": 2
                        },
                        "caveats": {"type": "array", "items": {"type": "string"}},
                        "tags": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["text", "title", "corpus_lines", "caveats", "tags"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["artifacts"],
        "additionalProperties": false
    })
}

/// Each judged field nullable, so a strict decoder can say "no judgement".
pub fn judged_artifacts_schema() -> serde_json::Value {
    let mut schema = artifacts_schema();
    schema["properties"]["moment"] = serde_json::json!({
        "type": ["object", "null"],
        "properties": {
            "intent": {"type": "string", "enum": ["remind", "none"]},
            "when": {"type": ["string", "null"]},
            "rule": {"type": ["string", "null"]}
        },
        "required": ["intent", "when", "rule"],
        "additionalProperties": false
    });
    schema["properties"]["events"] = serde_json::json!({
        "type": "array",
        "items": {"type": "string"}
    });
    schema["required"] = serde_json::json!(["artifacts", "moment", "events"]);
    schema
}

/// Models wrap JSON in fences and prose regardless of the prompt.
pub(crate) fn extract_json(body: &str) -> &str {
    let start = body.find('{');
    let end = body.rfind('}');
    match (start, end) {
        (Some(s), Some(e)) if e > s => &body[s..=e],
        _ => body,
    }
}

/// Parses objects one at a time so one bad object does not lose the rest; the caller flags the result as degraded.
fn salvage_objects(json: &str) -> Vec<RawArtifact> {
    let Some(start) = json.find("\"artifacts\"") else {
        return Vec::new();
    };
    let Some(open) = json[start..].find('[').map(|i| i + start) else {
        return Vec::new();
    };

    let bytes = json.as_bytes();
    let (mut depth, mut in_string, mut escaped) = (0i32, false, false);
    let mut object_start: Option<usize> = None;
    let mut out = Vec::new();
    for (i, &b) in bytes.iter().enumerate().skip(open + 1) {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => {
                if depth == 0 {
                    object_start = Some(i);
                }
                depth += 1;
            }
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = object_start.take()
                    && let Ok(raw) = serde_json::from_str::<RawArtifact>(&json[s..=i])
                {
                    out.push(raw);
                }
            }
            b']' if !in_string && depth == 0 => break,
            _ => {}
        }
    }
    out
}

/// Enough to see the shape without pasting a segment of someone's corpus into a log.
const RAW_ON_FAILURE: usize = 800;

pub fn parse_response(body: &str) -> Result<Vec<ProposedArtifact>> {
    let json = extract_json(body);
    let env: Envelope = match serde_json::from_str(json) {
        Ok(env) => env,
        Err(e) => {
            let objects = salvage_objects(json);
            if objects.is_empty() {
                // Debug, not warn: model output carries corpus text.
                tracing::debug!(
                    error = %e,
                    raw = %json.chars().take(RAW_ON_FAILURE).collect::<String>(),
                    "synthesizer output could not be parsed or salvaged"
                );
                return Err(Error::MalformedLlmOutput(e.to_string()));
            }
            tracing::warn!(
                error = %e,
                artifacts = objects.len(),
                "synthesizer output was cut off or malformed; keeping the artifacts that parsed"
            );
            Envelope { artifacts: objects }
        }
    };

    proposed_from(env.artifacts, false)
}

/// `judged` gates model tags: off for promotions, where tags drift.
fn proposed_from(raws: Vec<RawArtifact>, judged: bool) -> Result<Vec<ProposedArtifact>> {
    let artifacts: Vec<ProposedArtifact> = raws
        .into_iter()
        .filter(|c| !c.text.trim().is_empty())
        .map(|c| ProposedArtifact {
            text: c.text.trim().to_string(),
            title: c.title.filter(|t| !t.trim().is_empty()),
            tags: if judged {
                c.tags
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.trim().to_lowercase())
                    .filter(|t| !t.is_empty())
                    .take(3)
                    .collect()
            } else {
                Vec::new()
            },
            corpus_lines: match c.corpus_lines.as_deref() {
                Some([a, b]) => Some((*a, *b)),
                _ => None,
            },
            // Capped at the three the prompt asks for; the tail is the least grounded.
            caveats: c
                .caveats
                .into_iter()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .take(3)
                .collect(),
        })
        .collect();

    if artifacts.is_empty() {
        return Err(Error::MalformedLlmOutput(
            "model returned no usable artifacts".into(),
        ));
    }
    Ok(artifacts)
}

/// A bad judgement never fails good artifacts, and a salvaged reply carries no judgement: half-read intent is worse.
pub fn parse_judged_response(body: &str) -> Result<crate::infer::SegmentReply> {
    let json = extract_json(body);
    match serde_json::from_str::<JudgedEnvelope>(json) {
        Ok(env) => {
            let judgement = crate::infer::Judgement {
                intent: env.moment.as_ref().and_then(|m| m.intent.clone()),
                when: env.moment.as_ref().and_then(|m| m.when.clone()),
                rule: env.moment.as_ref().and_then(|m| m.rule.clone()),
                events: env.events.unwrap_or_default(),
            };
            // Empty (or blank-only) artifacts are a real answer here: a reminder-only note leaves nothing the prompt allows
            // writing, so the judgement alone is the reply. Judged by text, since small models send a blank placeholder.
            let nothing_to_write = env.artifacts.iter().all(|c| c.text.trim().is_empty());
            let artifacts = if nothing_to_write && judgement.says_something() {
                Vec::new()
            } else {
                proposed_from(env.artifacts, true)?
            };
            Ok(crate::infer::SegmentReply {
                artifacts,
                judgement: Some(judgement),
            })
        }
        Err(e) => {
            let objects = salvage_objects(json);
            if objects.is_empty() {
                tracing::debug!(
                    error = %e,
                    raw = %json.chars().take(RAW_ON_FAILURE).collect::<String>(),
                    "judged synthesizer output could not be parsed or salvaged"
                );
                return Err(Error::MalformedLlmOutput(e.to_string()));
            }
            tracing::warn!(
                error = %e,
                artifacts = objects.len(),
                "judged output was cut off or malformed; keeping the artifacts, dropping the judgement"
            );
            Ok(crate::infer::SegmentReply {
                artifacts: proposed_from(objects, true)?,
                judgement: None,
            })
        }
    }
}

pub const DESCRIBE_SYSTEM: &str = r#"You read images for a personal knowledge base and write down everything in them worth keeping, as markdown.

Rules:
- Transcribe any visible text faithfully and completely. Keep its structure: headings as headings, lists as lists, tables as markdown tables, code as code blocks. Do not correct, summarize or reorder it.
- Write formulas, whether transcribed or read off a picture, in LaTeX: `$…$` inline, `$$…$$` on a line of their own; never `\(…\)` or `\[…\]`.
- Where there is no text, or beside it, describe what is shown: diagrams (their parts and how they connect), charts (axes, series, the values that can be read), scenes, objects, people's roles if evident, places, labels, brands, numbers, dates. Name what is identifiable.
- Prefer specifics over impressions. Do not pad, do not speculate beyond what is visible, do not add advice.
- You may be given context about the capture: a note from the person who took it, when and where it was taken, the device. Where it is relevant, weave it in naturally so the text can be found again by it — as a short opening line or where it explains the content — but do not repeat it mechanically or invent detail around it.
- Output markdown only. No preamble, no closing remarks, no mention of these instructions."#;

/// Language here, not in a translated `DESCRIBE_SYSTEM`: transcribed text stays untranslated, the prose does not.
pub fn describe_context(metadata: &serde_json::Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    let lang = crate::infer::lang::of_corpus(metadata);
    if lang != crate::infer::lang::Lang::En {
        lines.push(format!(
            "Write your description in {} ({}). Text that is visible in the image \
             is still transcribed exactly as it appears there, in whatever language \
             it is written in — only your own prose is in {}.",
            lang.endonym(),
            lang.tag(),
            lang.endonym()
        ));
    }
    if let Some(note) = metadata["note"].as_str().filter(|n| !n.trim().is_empty()) {
        // This copy leads the prompt and costs tokens; the stored note is whole.
        lines.push(format!(
            "Context from the person who captured this: {}",
            note.trim()
                .chars()
                .take(crate::core::ingest::MAX_NOTE_CHARS)
                .collect::<String>()
        ));
    }
    let mut facts: Vec<String> = Vec::new();
    let exif = &metadata["exif"];
    if let Some(t) = exif["taken_at"].as_str() {
        facts.push(format!("taken {t}"));
    }
    if let (Some(lat), Some(lon)) = (exif["gps"]["lat"].as_f64(), exif["gps"]["lon"].as_f64()) {
        facts.push(format!("GPS {lat:.4},{lon:.4}"));
    }
    if let Some(c) = exif["camera"].as_str() {
        facts.push(format!("device {c}"));
    }
    if let Some(n) = metadata["file"]["name"].as_str() {
        facts.push(format!("file {n}"));
    }
    if !facts.is_empty() {
        lines.push(format!("Capture facts: {}.", facts.join(", ")));
    }
    lines.push("Read the image and write down everything worth keeping.".into());
    lines.join("\n")
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_synthesis_reply_parses_into_a_draft() {
        let body = r#"{"merged":{"title":"Praxis","text":"Alles zusammen.","category":"reference","caveats":[]}}"#;
        let d = parse_synthesis(body).expect("a well-formed draft parses");
        assert_eq!(d.title.as_deref(), Some("Praxis"));
        assert_eq!(d.text, "Alles zusammen.");
        assert!(d.caveats.is_empty());
    }

    #[test]
    fn a_synthesis_reply_with_no_text_is_an_error() {
        let body =
            r#"{"merged":{"title":"Praxis","text":"   ","category":"reference","caveats":[]}}"#;
        assert!(parse_synthesis(body).is_err(), "an empty body is no draft");
    }

    #[test]
    fn a_synthesis_reply_in_a_fence_still_parses() {
        let body = "```json\n{\"merged\":{\"title\":\"P\",\"text\":\"x\",\"category\":\"reference\",\"caveats\":[]}}\n```";
        assert!(
            parse_synthesis(body).is_ok(),
            "fences are stripped like everywhere else"
        );
    }

    #[test]
    fn the_synthesis_schema_requires_every_field_it_lists() {
        let s = synthesize_schema();
        let m = &s["properties"]["merged"];
        let required = m["required"].as_array().expect("required is a list");
        for f in ["text", "title", "category", "caveats"] {
            assert!(
                required.iter().any(|r| r.as_str() == Some(f)),
                "{f} must be required"
            );
        }
        assert_eq!(m["additionalProperties"], serde_json::json!(false));
    }
    use super::*;

    #[test]
    fn every_language_says_the_judgement_is_not_an_artifact() {
        use crate::infer::lang::Lang;
        let phrase = [
            (Lang::En, "The judgement is not an artifact"),
            (Lang::De, "Das Urteil ist kein Artefakt"),
            (Lang::Es, "El juicio no es un artefacto"),
            (Lang::Fr, "Le jugement n'est pas un artefact"),
            (Lang::It, "Il giudizio non è un artefatto"),
            (Lang::Nl, "Het oordeel is geen artefact"),
            (Lang::Pl, "Ocena nie jest artefaktem"),
            (Lang::Pt, "O julgamento não é um artefato"),
            (Lang::Ru, "Суждение — не артефакт"),
            (Lang::Tr, "Yargı bir artefakt değildir"),
        ];
        for (lang, p) in phrase {
            assert!(
                synthesizer_system(lang).contains(p),
                "{lang:?} prompt lacks the one-artifact rule"
            );
        }
    }

    #[test]
    fn an_empty_need_parses_as_nothing_further() {
        assert!(parse_plan(r#"{"need": []}"#).is_empty());
        assert!(parse_plan(r#"{"need": null}"#).is_empty());
        assert!(parse_plan(r#"{"need": ""}"#).is_empty());
        assert!(parse_plan(r#"{"need": ["   ", ""]}"#).is_empty());
    }

    #[test]
    fn a_reply_that_is_not_the_shape_asked_for_is_nothing_further() {
        assert!(parse_plan("I think you need more on tickers").is_empty());
        assert!(parse_plan(r#"{"query": "tickers"}"#).is_empty());
        assert!(parse_plan(r#"{"need": {"q": "tickers"}}"#).is_empty());
        assert!(parse_plan("").is_empty());
    }

    #[test]
    fn a_need_parses_as_the_queries_to_run() {
        assert_eq!(
            parse_plan(r#"{"need": ["retention ticker interval", "backup schedule"]}"#),
            vec![
                "retention ticker interval".to_string(),
                "backup schedule".to_string()
            ]
        );
        assert_eq!(
            parse_plan("Here you go:\n```json\n{\"need\": [\"ticker interval\"]}\n```"),
            vec!["ticker interval".to_string()]
        );
    }

    #[test]
    fn a_bare_string_reads_as_the_one_query_it_is() {
        assert_eq!(
            parse_plan(r#"{"need": "ticker interval"}"#),
            vec!["ticker interval".to_string()]
        );
    }

    #[test]
    fn repeats_are_dropped_and_the_plan_is_capped() {
        assert_eq!(
            parse_plan(r#"{"need": ["tickers", "TICKERS", " tickers "]}"#),
            vec!["tickers".to_string()]
        );
        assert_eq!(
            parse_plan(r#"{"need": ["a", "b", "c", "d", "e"]}"#).len(),
            PLAN_MAX_QUERIES
        );
    }

    #[test]
    fn a_reply_that_satisfies_the_plan_schema_parses() {
        let schema = plan_schema();
        assert_eq!(
            schema["properties"]["need"]["type"], "array",
            "the grammar must be able to name more than one missing subject"
        );
        assert!(
            schema["properties"]["need"]["maxItems"].is_null(),
            "an endpoint whose grammar compiler cannot express maxItems refuses \
             the whole call; the cap is the parser's job"
        );
        assert_eq!(
            parse_plan(r#"{"need":["a","b","c","d","e"]}"#).len(),
            PLAN_MAX_QUERIES,
            "the parser is now the only thing holding the fan-out to its width"
        );
        assert!(!parse_plan(r#"{"need":["x"]}"#).is_empty());
        assert!(
            parse_plan(r#"{"need":[]}"#).is_empty(),
            "the empty list is the common answer and must parse as one"
        );
    }

    #[test]
    fn the_schemas_ask_for_no_category_no_pin_and_no_links() {
        let required = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        let plain = artifacts_schema();
        assert_eq!(
            required(&plain["properties"]["artifacts"]["items"]["required"]),
            ["text", "title", "corpus_lines", "caveats", "tags"]
        );
        let judged = judged_artifacts_schema();
        assert_eq!(
            required(&judged["required"]),
            ["artifacts", "moment", "events"]
        );
        for schema in [plain, judged] {
            let text = schema.to_string();
            for field in ["category", "pinned", "links"] {
                assert!(
                    !text.contains(field),
                    "a schema still asks for {field}: {text}"
                );
            }
        }
    }

    #[test]
    fn a_category_off_the_list_becomes_other() {
        assert_eq!(
            normalize_category("Forensic Science / Criminalistics"),
            "other"
        );
        assert_eq!(normalize_category("System Administration"), "other");
        assert_eq!(normalize_category(""), "other");
    }

    #[test]
    fn a_category_on_the_list_survives_case_and_padding() {
        assert_eq!(normalize_category("Procedure"), "procedure");
        assert_eq!(normalize_category("  snippet "), "snippet");
        assert_eq!(normalize_category("reference"), "reference");
    }

    #[test]
    fn context_blocks_are_fenced_and_labelled_as_context_only() {
        use crate::infer::context::WindowContext;

        let ctx = WindowContext {
            opening: Some("# Guide\nPBS 3.x on Debian 12.".into()),
            before: Some("previous window tail".into()),
            after: Some("next window head".into()),
        };
        let p = user_prompt("the window body", 1, 1024, &ctx, None);

        assert!(p.contains("PBS 3.x on Debian 12."));
        assert!(p.contains("previous window tail"));
        assert!(p.contains("next window head"));
        assert!(p.contains("----- INPUT -----\nthe window body\n----- END INPUT -----"));

        let opening_at = p.find("PBS 3.x").unwrap();
        let before_at = p.find("previous window tail").unwrap();
        let input_at = p.find("----- INPUT -----").unwrap();
        let after_at = p.find("next window head").unwrap();
        assert!(opening_at < before_at && before_at < input_at && input_at < after_at);
    }

    #[test]
    fn an_empty_context_renders_exactly_the_prompt_of_before() {
        use crate::infer::context::WindowContext;

        let p = user_prompt("body", 1, 1024, &WindowContext::default(), None);
        assert!(
            !p.contains("context only"),
            "an empty context must not emit empty fences: {p}"
        );
        assert!(p.starts_with("The input below starts at line 1."));
        assert!(p.ends_with("----- END INPUT -----"));
    }

    #[test]
    fn the_translations_carry_the_same_contract() {
        use crate::infer::lang::Lang;

        // Every literal the parser, the schema or `user_prompt` depends on.
        const CONTRACT: &[&str] = &[
            r#"{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}"#,
            "corpus_lines",
            "caveats",
            r#"{"intent":"remind"|"none","when":...,"rule":...}"#,
            "context only",
            "INPUT",
            "NEIGHBORS",
            "JUDGE",
            "moment",
            "events",
            "RRULE",
            "FREQ, INTERVAL, BYDAY",
            "BYMONTHDAY, UNTIL, COUNT",
            "ISO-8601",
            "2026-09-04T09:00",
            "09:00",
            "17:07",
            "`## `",
        ];
        // Translations wrap differently; `FREQ,\nINTERVAL` is the same contract as `FREQ, INTERVAL`.
        let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
        for lang in Lang::ALL {
            let p = flat(synthesizer_system(lang));
            for needle in CONTRACT {
                assert!(
                    p.contains(&flat(needle)),
                    "the {} prompt lost `{needle}`",
                    lang.tag()
                );
            }
            // What the writer no longer answers: the decision model reads and links instead.
            for gone in [
                "pinned",
                r#"{"artifact_id":"...","reason":"..."}"#,
                "link targets",
            ] {
                assert!(
                    !p.contains(gone),
                    "the {} prompt still carries `{gone}`",
                    lang.tag()
                );
            }
            assert!(!title_system(lang).is_empty());
        }

        // A `match` arm pointing at the wrong constant is otherwise invisible.
        let seen: std::collections::BTreeSet<&str> =
            Lang::ALL.iter().map(|l| synthesizer_system(*l)).collect();
        assert_eq!(seen.len(), Lang::ALL.len(), "two languages share a prompt");
        let titles: std::collections::BTreeSet<&str> =
            Lang::ALL.iter().map(|l| title_system(*l)).collect();
        assert_eq!(titles.len(), Lang::ALL.len());
        assert_eq!(synthesizer_system(Lang::En), SYNTHESIZER_SYSTEM);
    }

    #[test]
    fn no_language_still_tells_the_writer_to_categorise_pin_or_link() {
        use crate::infer::lang::Lang;
        for lang in Lang::ALL {
            let p = synthesizer_system(lang);
            for field in ["\"category\"", "\"pinned\"", "\"links\""] {
                assert!(
                    !p.contains(field),
                    "the {} prompt still asks for {field}",
                    lang.tag()
                );
            }
        }
    }

    #[test]
    fn no_language_still_sends_a_relation_to_a_neighbour_to_the_judgement() {
        use crate::infer::lang::Lang;
        // The word each translation used for it; none has another use in its prompt.
        const RELATION: &[&str] = &[
            "relation to",
            "Beziehung",
            "relación",
            "relation à",
            "relazione",
            "relatie tot",
            "związek",
            "relação",
            "связь",
            "ilişkisini",
        ];
        assert_eq!(RELATION.len(), Lang::ALL.len());
        let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
        for lang in Lang::ALL {
            let p = flat(synthesizer_system(lang));
            for word in RELATION {
                assert!(
                    !p.contains(word),
                    "the {} prompt still names a relation to a neighbour (`{word}`)",
                    lang.tag()
                );
            }
        }
    }

    #[test]
    fn the_user_prompt_shows_neighbours_as_context_and_judges_no_links() {
        let ctx = crate::infer::context::WindowContext::default();
        let judge = crate::infer::JudgeAsk {
            now_local: "2026-09-04 09:00 (Friday)".into(),
            tz: "UTC".into(),
            forced_intent: None,
            neighbors: vec![crate::infer::Neighbor {
                id: "n-1".into(),
                title: Some("Invoices".into()),
                text: "the invoice workflow".into(),
            }],
        };
        let p = user_prompt("x", 1, 100, &ctx, Some(&judge));
        assert!(p.contains("----- NEIGHBORS (context only) -----"), "{p}");
        assert!(
            p.contains("the invoice workflow"),
            "the neighbour stays as context"
        );
        assert!(p.contains("Judge this note: moment, events.\n"), "{p}");
        assert!(!p.contains("link"), "{p}");
    }

    #[test]
    fn no_prompt_asks_for_a_language_any_more() {
        use crate::infer::lang::Lang;
        for lang in Lang::ALL {
            assert!(
                !synthesizer_system(lang).contains("language the input was written in"),
                "the {} prompt still asks instead of speaking",
                lang.tag()
            );
        }
    }

    #[test]
    fn the_system_prompt_forbids_extracting_from_context() {
        assert!(SYNTHESIZER_SYSTEM.contains("context only"));
        assert!(SYNTHESIZER_SYSTEM.contains("INPUT"));
    }

    #[test]
    fn a_reply_that_satisfies_the_artifact_schema_parses() {
        let required = artifacts_schema()["properties"]["artifacts"]["items"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        let reply = r#"{"artifacts":[{"text":"body","title":"A","category":"note",
            "tags":["t"],"pinned":false,"corpus_lines":[1,4],"caveats":["only on linux"]}]}"#;
        for field in &required {
            assert!(
                reply.contains(&format!("\"{field}\"")),
                "the schema requires {field}, which this test never proves parsable"
            );
        }
        let out = parse_response(reply).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].corpus_lines, Some((1, 4)));
        assert_eq!(out[0].caveats, vec!["only on linux".to_string()]);
    }

    #[test]
    fn every_judge_schema_object_is_closed_and_rooted_in_an_object() {
        fn closed(v: &serde_json::Value, path: &str) {
            if v["type"] == "object" {
                assert_eq!(
                    v["additionalProperties"],
                    serde_json::json!(false),
                    "{path} is an open object, so it accepts fields it never named"
                );
                let required: Vec<&str> = v["required"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.as_str())
                    .collect();
                for (k, sub) in v["properties"].as_object().into_iter().flatten() {
                    assert!(
                        required.contains(&k.as_str()),
                        "{path}.{k} is named but not required, which a strict schema refuses"
                    );
                    closed(sub, &format!("{path}.{k}"));
                }
            }
            // Into arrays too, or the one array of objects goes unchecked.
            if v.get("items").is_some() {
                closed(&v["items"], &format!("{path}[]"));
            }
            for (i, sub) in v["anyOf"].as_array().into_iter().flatten().enumerate() {
                closed(sub, &format!("{path}.anyOf[{i}]"));
            }
        }

        for (name, schema) in [
            ("gap_label", gap_label_schema()),
            ("synthesize", synthesize_schema()),
            ("plan", plan_schema()),
            ("artifacts", artifacts_schema()),
        ] {
            assert_eq!(
                schema["type"], "object",
                "the {name} schema is not rooted in an object"
            );
            closed(&schema, name);
        }
    }

    #[test]
    fn a_truncated_list_keeps_the_artifacts_that_finished() {
        // What a small model emits out of budget: two complete objects, a third cut mid-string.
        let cut = r###"{"artifacts":[
          {"text":"first complete","title":"one","tags":[],"corpus_lines":[1,2]},
          {"text":"second complete","title":"two","tags":[]},
          {"text":"third was cut off here"###;
        let out = parse_response(cut).expect("the finished artifacts must survive");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "first complete");
        assert_eq!(out[1].text, "second complete");
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_a_chunk_early() {
        let cut = r###"{"artifacts":[
          {"text":"awk '{print $1}' file.txt","title":"awk","tags":[]},
          {"text":"cut off"###;
        let out = parse_response(cut).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "awk '{print $1}' file.txt");
    }

    #[test]
    fn a_response_cut_before_any_chunk_closed_is_still_an_error() {
        let cut = r###"{"artifacts":[{"text":"nothing finished"###;
        assert!(parse_response(cut).is_err());
    }

    #[test]
    fn one_malformed_object_costs_only_itself() {
        let broken = r###"{"artifacts":[
          {"text":"first good","title":"one","tags":[]},
          {"text":"middle bad" "title":"two","tags":[]},
          {"text":"third good","title":"three","tags":[]}
        ]}"###;
        let out = parse_response(broken).expect("the parsable objects must survive");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "first good");
        assert_eq!(out[1].text, "third good");
    }

    #[test]
    fn a_bad_escape_does_not_cost_the_objects_before_it() {
        // An unescaped quote derails the scanner; objects already closed must still be returned.
        let broken = r###"{"artifacts":[
          {"text":"first good","title":"one","tags":[]},
          {"text":"he said "stop" here","title":"two","tags":[]}
        ]}"###;
        let out = parse_response(broken).expect("the objects before the fault must survive");
        assert!(!out.is_empty(), "salvage returned nothing at all");
        assert_eq!(out[0].text, "first good");
    }

    #[test]
    fn a_reply_with_no_parsable_object_is_still_an_error() {
        // A segment producing nothing must fail, so it is recorded as missing rather than dropped.
        let prose = r###"{"artifacts":[{"text":"unterminated and "broken, "tags":}]}"###;
        assert!(parse_response(prose).is_err());
    }

    #[test]
    fn caveats_are_parsed_when_the_model_supplies_them() {
        let body = r#"{"artifacts":[{
            "text":"Run `mkfs.ext4 /dev/sdb1` to format the partition.",
            "title":"Formatting a partition",
            "category":"procedure",
            "tags":["disk"],
            "corpus_lines":[3,9],
            "caveats":["Destroys every existing file on the device.",
                       "Requires root."]
        }]}"#;
        let got = parse_response(body).unwrap();
        assert_eq!(
            got[0].caveats,
            vec![
                "Destroys every existing file on the device.".to_string(),
                "Requires root.".to_string()
            ]
        );
    }

    #[test]
    fn an_artifact_without_caveats_parses_to_an_empty_list() {
        let body = r#"{"artifacts":[{"text":"plain","title":"t","category":"c","tags":[]}]}"#;
        assert!(parse_response(body).unwrap()[0].caveats.is_empty());
    }

    #[test]
    fn the_system_prompt_asks_for_caveats_and_forbids_inventing_them() {
        assert!(SYNTHESIZER_SYSTEM.contains("caveats"));
        assert!(
            SYNTHESIZER_SYSTEM.contains("Never invent"),
            "the prompt must tie caveats to what the source says"
        );
    }

    // r###: the JSON contains the sequence `"##` (a quoted markdown H2),
    // which would terminate both an r#"..."# and an r##"..."## literal.
    const GOOD: &str = r###"{"artifacts":[
      {"text":"## Mount an image\nRun `ewfmount evidence.E01 /mnt/ewf`.",
       "title":"Mount an E01 image","category":"procedure",
       "tags":["forensics","linux"],"corpus_lines":[3,9]}
    ]}"###;

    #[test]
    fn parses_a_well_formed_response() {
        let out = parse_response(GOOD).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title.as_deref(), Some("Mount an E01 image"));
        assert!(out[0].tags.is_empty());
        assert_eq!(out[0].corpus_lines, Some((3, 9)));
    }

    #[test]
    fn strips_code_fences_models_add_anyway() {
        let fenced = format!("Here you go:\n```json\n{GOOD}\n```\n");
        assert_eq!(parse_response(&fenced).unwrap().len(), 1);
    }

    #[test]
    fn missing_optional_fields_are_tolerated() {
        let minimal = r#"{"artifacts":[{"text":"bare text"}]}"#;
        let out = parse_response(minimal).unwrap();
        assert_eq!(out[0].text, "bare text");
        assert!(out[0].title.is_none());
        assert!(out[0].tags.is_empty());
        assert!(out[0].corpus_lines.is_none());
    }

    #[test]
    fn malformed_json_is_a_retryable_error() {
        let e = parse_response("not json at all").unwrap_err();
        assert!(matches!(e, crate::error::Error::MalformedLlmOutput(_)));
        assert!(e.retryable());
    }

    #[test]
    fn empty_chunk_list_is_rejected() {
        assert!(parse_response(r#"{"artifacts":[]}"#).is_err());
    }

    #[test]
    fn blank_chunk_texts_are_dropped_not_stored() {
        let body = r#"{"artifacts":[{"text":"real"},{"text":"   "}]}"#;
        let out = parse_response(body).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn code_fences_inside_artifact_text_survive_extraction() {
        let body =
            r#"{"artifacts":[{"text":"Run:\n```bash\nawk '{print $1}' file\n```","title":"awk"}]}"#;
        let out = parse_response(body).unwrap();
        assert!(
            out[0].text.contains("awk '{print $1}' file"),
            "code mangled: {}",
            out[0].text
        );
    }

    #[test]
    fn a_non_array_source_lines_is_ignored_rather_than_fatal() {
        let body = r#"{"artifacts":[{"text":"t","corpus_lines":[1,2,3]}]}"#;
        assert_eq!(parse_response(body).unwrap()[0].corpus_lines, None);
    }

    #[test]
    fn system_prompt_states_the_hard_rules() {
        assert!(SYNTHESIZER_SYSTEM.contains("VERBATIM"));
        assert!(SYNTHESIZER_SYSTEM.contains("markdown"));
        assert!(SYNTHESIZER_SYSTEM.contains("H1") || SYNTHESIZER_SYSTEM.contains("`#`"));
    }

    #[test]
    fn repair_prompt_includes_the_parse_error() {
        let p = repair_prompt("{bad", "expected value at line 1");
        assert!(p.contains("expected value at line 1"));
        assert!(p.contains("{bad"));
    }

    #[test]
    fn describe_context_bounds_the_note_it_spends_on_a_model_call() {
        let long = "x".repeat(crate::core::ingest::MAX_NOTE_CHARS + 500);
        let m = serde_json::json!({ "note": long });
        let ctx = describe_context(&m);
        let kept = ctx
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("Context from the person who captured this: ");
        assert_eq!(kept.chars().count(), crate::core::ingest::MAX_NOTE_CHARS);
    }

    #[test]
    fn describe_context_leads_with_the_note_then_the_facts_and_omits_what_is_absent() {
        let m = serde_json::json!({
            "note": "whiteboard from Tuesday planning",
            "file": {"name": "IMG_2041.jpeg"},
            "exif": {"taken_at": "2026-08-09T14:12:03", "camera": "Apple iPhone 15",
                     "gps": {"lat": 48.2082, "lon": 16.3738}}
        });
        let ctx = describe_context(&m);
        let note_at = ctx.find("whiteboard from Tuesday planning").unwrap();
        let taken_at = ctx.find("2026-08-09T14:12:03").unwrap();
        assert!(note_at < taken_at, "{ctx}");
        assert!(ctx.contains("48.2082"), "{ctx}");
        assert!(ctx.contains("Apple iPhone 15"));
        assert!(ctx.contains("IMG_2041.jpeg"));

        let bare = describe_context(&serde_json::json!({}));
        assert!(!bare.contains("taken"), "{bare}");
        assert!(!bare.contains("GPS"), "{bare}");
        assert!(bare.contains("Read the image"), "{bare}");
    }

    #[test]
    fn a_description_is_written_in_the_language_the_capture_was_stamped_with() {
        let de = describe_context(&serde_json::json!({ "lang": "de" }));
        assert!(
            de.starts_with("Write your description in Deutsch (de)."),
            "{de}"
        );
        assert!(
            de.contains("transcribed exactly as it appears"),
            "the transcription must not be swept along: {de}"
        );

        // English adds no line: tokens for nothing on every image capture.
        for m in [
            serde_json::json!({ "lang": "en" }),
            serde_json::json!({}),
            serde_json::json!({ "lang": "kl" }),
        ] {
            let ctx = describe_context(&m);
            assert!(!ctx.contains("Write your description in"), "{ctx}");
        }
    }

    #[test]
    fn an_answer_that_opens_with_the_sentinel_is_an_abstention() {
        assert!(abstained(
            "Not in the knowledge base. Nothing covers mounting E01 images."
        ));
        assert!(abstained(
            "  not in the knowledge base — the excerpts are about FAT."
        ));
        assert!(abstained(
            "**Not in the knowledge base.** The excerpts describe…"
        ));
        assert!(abstained("# Not in the knowledge base\n\nThe excerpts…"));
        // A non-ASCII lead-in puts the prefix off a byte boundary.
        assert!(abstained("„Not in the knowledge base.“ Nothing covers it."));
        assert!(abstained(
            "• Not in the knowledge base — the excerpts are about FAT."
        ));
        assert!(abstained("- Not in the knowledge base."));
        assert!(abstained("1. Not in the knowledge base."));
        assert!(abstained("\"Not in the knowledge base\""));
    }

    #[test]
    fn an_answer_that_quotes_the_phrase_to_argue_with_it_is_not_an_abstention() {
        // Scored as an abstention this would record a gap for a question the base answered.
        assert!(!abstained(
            "\"Not in the knowledge base\" is the wrong read here; excerpt 3 covers it."
        ));
        assert!(!abstained(
            "“Not in the knowledge base” would be wrong — see excerpt 1."
        ));
    }

    #[test]
    fn an_answer_that_merely_mentions_the_phrase_is_not_an_abstention() {
        assert!(!abstained(
            "Mount it with `ewfmount`. (Details on E02 are not in the knowledge base.)"
        ));
        assert!(!abstained(""));
        assert!(!abstained(
            "Not in the manual, but in the excerpts: use -o ro."
        ));
    }

    #[test]
    fn every_prompt_written_as_markdown_names_the_math_delimiters() {
        let mut prompts = vec![
            ASK_SYSTEM,
            GENERATE_SYSTEM,
            RESCUE_SYSTEM,
            SYNTHESIZE_SYSTEM,
            DESCRIBE_SYSTEM,
        ];
        prompts.extend(crate::infer::lang::Lang::ALL.map(synthesizer_system));
        for p in prompts {
            assert!(
                p.contains("`$$…$$`") && p.contains(r"`\(…\)`") && p.contains(r"`\[…\]`"),
                "{p}"
            );
        }
    }

    #[test]
    fn the_system_prompt_tells_the_model_the_exact_sentinel_the_code_reads() {
        assert!(ASK_SYSTEM.contains(ABSTAIN_PREFIX), "{ASK_SYSTEM}");
    }

    #[test]
    fn a_gap_label_is_read_out_of_the_envelope_and_tidied() {
        assert_eq!(
            parse_gap_label(r#"{"label": "\"Forensic image mounting.\""}"#).unwrap(),
            "Forensic image mounting"
        );
        assert!(parse_gap_label(r#"{"label": ""}"#).is_err());
        assert!(parse_gap_label("nope").is_err());
    }

    #[test]
    fn naming_a_gap_is_shown_a_bounded_number_of_bounded_questions() {
        let long = "x".repeat(GAP_LABEL_MAX_CHARS + 50);
        let mut qs: Vec<&str> = vec![long.as_str(); GAP_LABEL_MAX_QUESTIONS + 20];
        qs[0] = "the newest question, which the caller passes first";
        let p = gap_label_prompt(&qs);
        assert_eq!(
            p.lines().count(),
            GAP_LABEL_MAX_QUESTIONS + 1,
            "one header and {GAP_LABEL_MAX_QUESTIONS} questions: {p}"
        );
        assert!(
            p.contains("the newest question"),
            "the cap keeps the newest, which the caller passes first: {p}"
        );
        assert!(p.contains('…'), "an overlong question is cut: {p}");
        assert!(
            !p.contains(&long),
            "no question reaches the prompt at full length"
        );
    }

    #[test]
    fn ask_prompt_skips_an_empty_excerpt() {
        let p = ask_prompt("q", &["[1] t\na".into(), String::new(), "[3] t\nc".into()]);
        assert_eq!(p, "Question: q\n\nExcerpts:\n\n[1] t\na\n\n---\n\n[3] t\nc");
    }

    #[test]
    fn a_pinned_flag_of_the_wrong_shape_costs_no_artifact() {
        for v in ["null", "\"false\"", "0", "[]"] {
            let body = format!(
                r#"{{"artifacts":[{{"text":"Send the invoice","title":"Invoice",
                   "category":"other","corpus_lines":[1,1],"caveats":[],
                   "tags":[],"pinned":{v}}}]}}"#
            );
            let r = parse_response(&body).unwrap_or_else(|e| panic!("pinned={v}: {e}"));
            assert_eq!(r.len(), 1, "pinned={v}");
            let j = parse_judged_response(&body).unwrap_or_else(|e| panic!("pinned={v}: {e}"));
            assert_eq!(j.artifacts.len(), 1, "pinned={v}");
        }
    }

    #[test]
    fn a_judged_reply_parses_moment_events_and_tags() {
        let body = r#"{"moment":{"intent":"remind","when":"2026-09-04T09:00","rule":null},
            "events":["2026-09-12T00:00"],
            "artifacts":[{"text":"Send the invoice","title":"Invoice",
                          "corpus_lines":[1,1],"caveats":[],"tags":["Billing "]}]}"#;
        let r = parse_judged_response(body).unwrap();
        let j = r.judgement.unwrap();
        assert_eq!(j.intent.as_deref(), Some("remind"));
        assert_eq!(j.when.as_deref(), Some("2026-09-04T09:00"));
        assert_eq!(j.rule, None);
        assert_eq!(j.events, vec!["2026-09-12T00:00"]);
        assert_eq!(r.artifacts[0].corpus_lines, Some((1, 1)));
        assert_eq!(
            r.artifacts[0].tags,
            vec!["billing"],
            "trimmed and lowercased"
        );
    }

    #[test]
    fn a_reply_that_still_carries_the_old_fields_parses() {
        // An older or stubborn model answers the shape it was once asked for.
        let body = r#"{"moment":{"intent":"none","when":null,"rule":null},"events":[],
            "links":[{"artifact_id":"a-1","reason":"same migration"}],
            "artifacts":[{"text":"Send the invoice","title":"Invoice","category":"procedure",
                          "corpus_lines":[1,1],"caveats":[],"tags":["billing"],"pinned":true}]}"#;
        let r = parse_judged_response(body).unwrap();
        assert_eq!(r.artifacts.len(), 1);
        assert_eq!(r.artifacts[0].text, "Send the invoice");
        assert_eq!(r.artifacts[0].tags, vec!["billing"]);
        assert_eq!(
            r.judgement.unwrap(),
            crate::infer::Judgement {
                intent: Some("none".into()),
                ..Default::default()
            },
            "the links are not read"
        );
        let plain = parse_response(body).unwrap();
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].title.as_deref(), Some("Invoice"));
        // Fields of the wrong shape cost nothing either.
        let odd = r#"{"artifacts":[{"text":"x","category":7,"pinned":"yes"}],"links":"none"}"#;
        assert_eq!(parse_judged_response(odd).unwrap().artifacts.len(), 1);
        assert_eq!(parse_response(odd).unwrap().len(), 1);
    }

    #[test]
    fn a_missing_or_malformed_judgement_never_fails_the_artifacts() {
        let plain = r#"{"artifacts":[{"text":"x"}]}"#;
        let r = parse_judged_response(plain).unwrap();
        assert_eq!(r.artifacts.len(), 1);
        assert_eq!(r.judgement.unwrap(), crate::infer::Judgement::default());
        let bad = r#"{"moment":"not an object","events":7,"links":{"no":"list"},
            "artifacts":[{"text":"x"}]}"#;
        let r = parse_judged_response(bad).unwrap();
        assert_eq!(r.artifacts.len(), 1);
        assert_eq!(r.judgement.unwrap(), crate::infer::Judgement::default());
    }

    #[test]
    fn a_reminder_note_that_is_only_a_judgement_keeps_its_judgement() {
        let body = r#"{"artifacts":[],
            "moment":{"intent":"remind","when":"2026-09-05T13:45","rule":null},
            "events":["2026-09-05T13:45"],"links":[]}"#;
        let r = parse_judged_response(body).unwrap();
        assert!(r.artifacts.is_empty());
        let j = r.judgement.expect("the judgement is the whole reply");
        assert_eq!(j.intent.as_deref(), Some("remind"));
        assert_eq!(j.when.as_deref(), Some("2026-09-05T13:45"));
    }

    #[test]
    fn a_blank_placeholder_artifact_is_the_same_answer_as_an_empty_list() {
        // A blank placeholder is what small models send instead of `[]`.
        let body = r#"{"artifacts":[{"text":"   ","title":""}],
            "moment":{"intent":"remind","when":"2026-09-05T13:45","rule":null},
            "events":[],"links":[]}"#;
        let r = parse_judged_response(body).unwrap();
        assert!(r.artifacts.is_empty());
        assert_eq!(
            r.judgement.expect("kept").when.as_deref(),
            Some("2026-09-05T13:45")
        );
        let empty = r#"{"artifacts":[{"text":"  "}],"moment":{"intent":"none","when":null,"rule":null},
            "events":[],"links":[]}"#;
        assert!(parse_judged_response(empty).is_err());
    }

    #[test]
    fn a_judged_reply_with_neither_artifacts_nor_a_judgement_is_still_malformed() {
        // The empty list means something only beside a judgement.
        let body = r#"{"artifacts":[],"moment":{"intent":"none","when":null,"rule":null},
            "events":[],"links":[]}"#;
        assert!(parse_judged_response(body).is_err());
        assert!(parse_judged_response(r#"{"artifacts":[]}"#).is_err());
    }

    #[test]
    fn a_judged_reply_that_omits_the_artifact_key_is_still_a_judgement() {
        let body = r#"{"moment":{"intent":"remind","when":"2026-09-04T09:00","rule":null}}"#;
        let r = parse_judged_response(body).expect("the judgement stands on its own");
        assert!(r.artifacts.is_empty());
        let j = r.judgement.expect("the moment was read");
        assert_eq!(j.intent.as_deref(), Some("remind"));
        assert_eq!(j.when.as_deref(), Some("2026-09-04T09:00"));
        assert!(parse_judged_response("{}").is_err());
    }

    #[test]
    fn the_plain_parse_takes_no_tags_from_the_model() {
        let body = r#"{"artifacts":[{"text":"x","tags":["topic"]}]}"#;
        let arts = parse_response(body).unwrap();
        assert!(arts[0].tags.is_empty());
    }
}
