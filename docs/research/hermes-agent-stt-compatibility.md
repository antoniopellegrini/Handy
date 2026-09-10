# Compatibilita tra Hermes Agent STT e il server Handy

Verifica svolta il 9 settembre 2026 sul commit Hermes Agent
`6f3e630b47e5afb709a7e30738d66f82bdaa2459` e sul commit Handy
`441504d0b1622990e27d32ecf0db215ba52f2279` del ramo
`feat/networked-inference`.

## Risposta breve

Hermes Agent puo inviare le trascrizioni a un endpoint OpenAI-compatible
personalizzato. La configurazione e `stt.provider: openai`, con
`stt.openai.base_url` e `stt.openai.api_key`.

Il supporto ai due valori YAML e entrato in Hermes `v2026.4.3`. Su versioni
precedenti la configurazione veniva ignorata. La conferma e nella
[discussione ufficiale della correzione](https://github.com/NousResearch/hermes-agent/pull/4103).

L'API batch di Handy ha la forma giusta, ma l'integrazione non e completa per i
messaggi vocali nel formato abituale delle piattaforme di messaggistica. Handy
accetta soltanto WAV, mentre Hermes inoltra normalmente il file originale, per
esempio Ogg/Opus. Le richieste con un file WAV funzionano, a condizione di usare
un ID modello realmente scaricato in Handy e il token corretto.

## Configurazione Hermes

Esempio per un server Handy raggiungibile dalla macchina o dal container che
esegue Hermes, sulla porta predefinita:

```yaml
stt:
  enabled: true
  provider: "openai"
  language: "it"
  openai:
    base_url: "http://<host-Handy>:8756/v1"
    api_key: "<token-server-Handy>"
    model: "small"
    language: "it"
```

Nel valore `base_url` va incluso `/v1`. L'SDK OpenAI aggiunge
`/audio/transcriptions`. `api_key` deve contenere il token generato da Handy,
perche Handy protegge tutti gli endpoint tranne `/health` con
`Authorization: Bearer <token>`.

Hermes documenta anche queste variabili d'ambiente:

```dotenv
STT_OPENAI_BASE_URL=http://<host-Handy>:8756/v1
STT_OPENAI_MODEL=small
VOICE_TOOLS_OPENAI_KEY=<token-server-Handy>
```

`OPENAI_API_KEY` e una seconda sorgente accettata per la chiave. In questo caso
preferirei la configurazione YAML: lega esplicitamente il token a STT e non
rischia di riusarlo per altre chiamate OpenAI. Inoltre, nella versione corrente
la variabile `STT_OPENAI_BASE_URL` viene letta all'avvio del processo. Dopo una
modifica del file `.env` bisogna riavviare Hermes. I valori YAML vengono invece
riletti per ogni chiamata. Il comportamento e descritto nel
[bug ufficiale #86071](https://github.com/NousResearch/hermes-agent/issues/86071).

Fonti Hermes:

- La [documentazione STT ufficiale](https://github.com/NousResearch/hermes-agent/blob/6f3e630b47e5afb709a7e30738d66f82bdaa2459/website/docs/user-guide/configuration.md#L2207-L2257)
  mostra `stt.provider`, `stt.openai.model`, la lingua e le variabili
  `STT_OPENAI_BASE_URL` e `STT_OPENAI_MODEL`.
- Il [resolver delle credenziali OpenAI audio](https://github.com/NousResearch/hermes-agent/blob/6f3e630b47e5afb709a7e30738d66f82bdaa2459/tools/transcription_cloud.py#L348-L416)
  legge `stt.openai.api_key` e `stt.openai.base_url`. Accetta una chiave vuota
  soltanto per URL riconosciuti come locali o privati, usando una chiave
  segnaposto per costruire il client. Questa eccezione non aiuta con Handy,
  perche Handy richiede comunque il suo token.
- Il [dispatcher STT](https://github.com/NousResearch/hermes-agent/blob/6f3e630b47e5afb709a7e30738d66f82bdaa2459/tools/transcription_tools.py)
  legge il modello da `stt.openai.model` e passa la richiesta al provider
  OpenAI incorporato.

## Richiesta HTTP effettiva

Hermes crea un client SDK OpenAI con la chiave e il `base_url` configurati, poi
chiama `client.audio.transcriptions.create`. La richiesta risultante e:

```http
POST /v1/audio/transcriptions
Authorization: Bearer <token-server-Handy>
Content-Type: multipart/form-data; boundary=...

file=<contenuto audio>
model=small
response_format=json
language=it
prompt=<opzionale>
```

Il [provider OpenAI di Hermes](https://github.com/NousResearch/hermes-agent/blob/6f3e630b47e5afb709a7e30738d66f82bdaa2459/tools/transcription_cloud.py#L107-L165)
invia sempre `file`, `model` e `response_format`. Invia `language` e `prompt`
quando configurati. Usa `response_format=text` solo con il modello
`whisper-1`; con altri ID, compresi quelli di Handy, chiede `json`. Accetta sia
una risposta testuale sia un oggetto o dizionario con il campo `text`.

Handy espone lo stesso `POST /v1/audio/transcriptions`, legge i campi `file`,
`model`, `language` e `response_format`, ignora i campi OpenAI che non puo
applicare, tra cui `prompt` e `temperature`, e risponde con `{"text":"..."}`
quando il formato richiesto e JSON. Vedi
[`openai.rs`](https://github.com/antoniopellegrini/Handy/blob/441504d0b1622990e27d32ecf0db215ba52f2279/src-tauri/src/server/openai.rs)
e la [registrazione delle route](https://github.com/antoniopellegrini/Handy/blob/441504d0b1622990e27d32ecf0db215ba52f2279/src-tauri/src/server/mod.rs#L282-L315).

## Compatibilita e limiti

### Parti compatibili

- Metodo, percorso, autenticazione Bearer e corpo multipart coincidono.
- `language` coincide. `it` viene inoltrato all'inferenza di Handy.
- La risposta JSON `{"text":"..."}` prodotta da Handy viene letta da Hermes.
- Il campo `prompt` non rompe la richiesta. Handy lo accetta e lo ignora.

### ID del modello

Il valore predefinito di Hermes, `whisper-1`, non e un ID standard del catalogo
Handy. Handy risponde 404 se il client chiede un modello sconosciuto o non
scaricato. Va quindi impostato un modello presente in `GET /v1/models`, per
esempio `small`, `medium`, `turbo` o `large` se e stato scaricato. Il catalogo
locale di Handy definisce questi ID in
[`model.rs`](https://github.com/antoniopellegrini/Handy/blob/441504d0b1622990e27d32ecf0db215ba52f2279/src-tauri/src/managers/model.rs#L546-L676),
mentre l'endpoint restituisce soltanto i modelli scaricati.

### Formato audio, il limite decisivo

Handy rifiuta con HTTP 415 qualsiasi upload che non abbia intestazione
RIFF/WAVE. Hermes dichiara e inoltra molti formati, tra cui MP3, M4A, WebM,
Ogg, Opus, AAC e FLAC. Converte preventivamente soltanto CAF verso WAV e i file
SILK verso WAV. Per il provider OpenAI prova una conversione di recupero verso
M4A soltanto dopo alcuni errori HTTP 400 restituiti come `BadRequestError`.

Questo recupero non risolve il caso Handy: la risposta e 415, non 400, e anche
il file M4A prodotto dal recupero sarebbe rifiutato. Quindi:

- un input gia WAV e compatibile;
- i normali vocali Ogg/Opus di Telegram, WhatsApp e altre piattaforme non sono
  compatibili senza una modifica;
- la modalita vocale che genera un WAV puo funzionare, ma va verificato il
  formato prodotto dalla specifica superficie Hermes usata.

Fonti:

- Hermes limita la preparazione iniziale ai casi speciali e inoltra gli altri
  formati al provider nel
  [modulo di preparazione audio](https://github.com/NousResearch/hermes-agent/blob/6f3e630b47e5afb709a7e30738d66f82bdaa2459/tools/transcription_audio.py).
- Il [retry del provider OpenAI](https://github.com/NousResearch/hermes-agent/blob/6f3e630b47e5afb709a7e30738d66f82bdaa2459/tools/transcription_cloud.py#L129-L160)
  scatta su `BadRequestError` e produce M4A.
- Handy controlla il magic number WAV e restituisce 415 negli altri casi in
  [`openai.rs`](https://github.com/antoniopellegrini/Handy/blob/441504d0b1622990e27d32ecf0db215ba52f2279/src-tauri/src/server/openai.rs#L158-L226).

## Conclusione operativa

Il collegamento diretto e valido per audio WAV. Non lo considererei ancora una
configurazione generale affidabile per i vocali di Hermes.

Le correzioni possibili sono due:

1. far accettare a Handy almeno Ogg/Opus, oppure transcodificare ogni upload in
   WAV sul server;
2. aggiungere in Hermes un provider o wrapper che converta sempre l'audio in
   WAV prima della chiamata OpenAI-compatible.

La prima soluzione rende l'endpoint Handy compatibile con piu client. La
seconda non richiede cambi al server, ma resta una personalizzazione locale di
Hermes.

## Aggiornamento: transcodifica lato Hermes

Hermes non espone un'opzione che obblighi il provider STT `openai` a
normalizzare ogni input. Il percorso incorporato inoltra il file originale e
ritenta con una conversione ffmpeg in M4A soltanto quando l'endpoint risponde
HTTP 400 con un errore che contiene `unsupported`, `corrupted` o `invalid
file`. Handy risponde HTTP 415 e non accetta neppure M4A, quindi questa logica
non risolve l'integrazione.

La tabella
[`Supported MEDIA: file extensions`](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/messaging/telegram.md#supported-media-file-extensions)
non descrive gli input STT. Documenta le estensioni che il gateway riconosce
nei tag `MEDIA:/path/to/file` prodotti dall'agente e che puo consegnare a
Telegram come allegati. Nella sezione `Incoming Voice` della stessa pagina,
Hermes specifica invece che conserva l'estensione originale ricevuta da
Telegram: `.ogg` per i vocali e, per esempio, `.mp3` o `.m4a` per gli allegati
audio. Il riconoscimento o il trasporto di queste estensioni non implica una
conversione prima della chiamata STT.

La soluzione supportata da Hermes e un provider STT a comando. La funzione e
presente dal commit
[`d3ffbc6409`](https://github.com/NousResearch/hermes-agent/commit/d3ffbc640940d8ce78ba2f5b44f0bc761e99dd45),
incluso per la prima volta nel tag `v2026.5.28`. La
[documentazione ufficiale](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/tts.md#stt-custom-command-providers)
definisce `stt.providers.<nome>`, i placeholder `{input_path}` e
`{output_path}`, e `env_passthrough` per le credenziali richieste dal comando.

Un provider di questo tipo puo eseguire sempre la sequenza seguente:

1. ffmpeg legge il file originale, inclusi Ogg/Opus di Telegram, MP3, M4A/MP4,
   WebM, AAC e FLAC;
2. ffmpeg produce WAV PCM mono a 16 kHz;
3. curl invia il WAV a `POST /v1/audio/transcriptions` di Handy e salva il
   testo in `{output_path}`.

Il comando va eseguito nel container Hermes, quindi `ffmpeg` e `curl` devono
essere installati nell'immagine. Questa configurazione risolve Hermes senza
modificare Handy, ma non rende il server compatibile con altri client che
inviino contenitori compressi. Una segnalazione Hermes ancora aperta,
[#81811](https://github.com/NousResearch/hermes-agent/issues/81811), conferma
che i provider STT a comando ricevono intenzionalmente l'audio originale e non
hanno un flag di normalizzazione automatica.
