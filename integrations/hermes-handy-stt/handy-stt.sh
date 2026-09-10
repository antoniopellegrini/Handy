#!/bin/sh

set -eu

program_name=${0##*/}

fail() {
  printf '%s: %s\n' "$program_name" "$1" >&2
  exit 1
}

if [ "$#" -ne 4 ]; then
  printf 'Usage: %s INPUT_PATH OUTPUT_PATH MODEL LANGUAGE\n' "$program_name" >&2
  exit 2
fi

input_path=$1
output_path=$2
model=$3
language=$4

: "${HANDY_STT_BASE_URL:?HANDY_STT_BASE_URL is required}"
: "${HANDY_STT_TOKEN:?HANDY_STT_TOKEN is required}"

command -v ffmpeg >/dev/null 2>&1 || fail "ffmpeg is not installed"
command -v curl >/dev/null 2>&1 || fail "curl is not installed"
command -v mktemp >/dev/null 2>&1 || fail "mktemp is not installed"

[ -f "$input_path" ] || fail "input is not a regular file: $input_path"
[ -r "$input_path" ] || fail "input is not readable: $input_path"
[ -n "$model" ] || fail "model is empty"
[ -n "$language" ] || fail "language is empty"
[ "$input_path" != "$output_path" ] || fail "input and output paths must differ"

output_dir=$(dirname -- "$output_path")
[ -d "$output_dir" ] || fail "output directory does not exist: $output_dir"
[ -w "$output_dir" ] || fail "output directory is not writable: $output_dir"

base_url=${HANDY_STT_BASE_URL%/}
case "$base_url" in
  */v1) transcription_url="$base_url/audio/transcriptions" ;;
  *) transcription_url="$base_url/v1/audio/transcriptions" ;;
esac

umask 077
temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/handy-stt.XXXXXX") ||
  fail "could not create a temporary directory"

cleanup() {
  rm -rf -- "$temporary_dir"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

wav_path=$temporary_dir/input.wav
response_path=$temporary_dir/transcript.txt
auth_header_path=$temporary_dir/auth-header

# Keep the bearer token out of curl's arguments and process listings.
printf 'Authorization: Bearer %s\n' "$HANDY_STT_TOKEN" >"$auth_header_path"

ffmpeg \
  -hide_banner \
  -loglevel error \
  -nostdin \
  -y \
  -i "$input_path" \
  -map 0:a:0 \
  -vn \
  -ac 1 \
  -ar 16000 \
  -c:a pcm_s16le \
  "$wav_path" || fail "ffmpeg could not convert the input audio"

curl \
  --fail \
  --silent \
  --show-error \
  --request POST \
  --header "@$auth_header_path" \
  --form "file=@$wav_path;type=audio/wav;filename=audio.wav" \
  --form-string "model=$model" \
  --form-string "language=$language" \
  --form-string "response_format=text" \
  --output "$response_path" \
  "$transcription_url" || fail "Handy rejected the transcription request"

[ -s "$response_path" ] || fail "Handy returned an empty transcript"

mv -f -- "$response_path" "$output_path" ||
  fail "could not write the transcript to: $output_path"
