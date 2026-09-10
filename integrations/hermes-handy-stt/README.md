# Trascrizione Hermes tramite Handy

Questo wrapper permette a Hermes Agent di trascrivere vocali Telegram e allegati audio tramite il server di Handy. Converte qualsiasi formato letto da `ffmpeg` in WAV PCM mono a 16 kHz, poi chiama l'endpoint OpenAI-compatible di Handy.

Richiede Hermes Agent `v2026.5.28` o successivo, perché usa un provider STT di tipo `command`. L'immagine Docker ufficiale recente contiene già `ffmpeg` e `curl`.

## Installazione su Docker101

Esegui i comandi dalla cartella del progetto Docker Compose di Hermes. Negli esempi il servizio si chiama `hermes`; sostituisci il nome se il tuo Compose usa `gateway` o un altro nome.

1. Copia questa directory nel progetto Compose e rendi eseguibile lo script:

   ```sh
   chmod 755 hermes-handy-stt/handy-stt.sh
   ```

2. Controlla versione e dipendenze nel container:

   ```sh
   docker compose exec hermes hermes version
   docker compose exec hermes ffmpeg -version
   docker compose exec hermes curl --version
   ```

   Se Hermes è più vecchio di `v2026.5.28`, aggiorna l'immagine prima di proseguire.

3. Aggiungi al servizio Hermes le voci di [compose.service.snippet.yaml](compose.service.snippet.yaml). Il bind mount rende disponibile lo script in sola lettura come `/usr/local/bin/handy-stt.sh`.

4. Inserisci `HANDY_STT_BASE_URL` e `HANDY_STT_TOKEN` nel file di ambiente protetto già usato dal deployment. [handy-stt.env.example](handy-stt.env.example) mostra il formato. Non usare `localhost` a meno che Handy non giri nello stesso container. Usa l'indirizzo LAN del computer che esegue Handy e limita i permessi del file:

   ```sh
   chmod 600 .env
   ```

5. Verifica il collegamento dal container. `/health` non richiede il token:

   ```sh
   docker compose run --rm --no-deps hermes sh -lc \
     'base=${HANDY_STT_BASE_URL%/}; base=${base%/v1}; curl --fail --silent --show-error "$base/health"'
   ```

   Elenca poi i modelli scaricati. Il token viene espanso dentro il container e non finisce nella cronologia della shell di Docker101:

   ```sh
   docker compose run --rm --no-deps hermes sh -lc \
     'base=${HANDY_STT_BASE_URL%/}; base=${base%/v1}; curl --fail --silent --show-error -H "Authorization: Bearer $HANDY_STT_TOKEN" "$base/v1/models"'
   ```

6. Unisci [config.yaml.snippet](config.yaml.snippet) nel `config.yaml` persistente di Hermes, normalmente montato in `/opt/data/config.yaml`. Sostituisci `REPLACE_WITH_A_DOWNLOADED_HANDY_MODEL_ID` con un ID restituito da `/v1/models`. Se esiste già una sezione `stt`, modifica quella invece di crearne una seconda.

7. Controlla la configurazione Compose e ricrea solo il servizio Hermes:

   ```sh
   docker compose config --quiet
   docker compose up -d --force-recreate hermes
   docker compose logs --tail 100 hermes
   ```

Invia infine un vocale Telegram. Se la trascrizione fallisce, i log mostreranno l'errore di conversione o la risposta HTTP, ma lo script non stampa il token.

## Prova diretta facoltativa

Per provare un file già accessibile nel container, passa percorso di ingresso, percorso di uscita, modello e lingua:

```sh
docker compose exec hermes sh -lc \
  '/usr/local/bin/handy-stt.sh /tmp/prova.ogg /tmp/prova.txt small it && cat /tmp/prova.txt'
```

Usa al posto di `small` l'ID trovato con `/v1/models`. Il wrapper restituisce un codice diverso da zero se il file è illeggibile, `ffmpeg` fallisce, Handy rifiuta autenticazione o modello, oppure la trascrizione è vuota. In questi casi Hermes non scambia il messaggio di errore per una trascrizione.
