# Fixing a translation

The AmberSight web app can be shown in ten languages besides English: German, French, Dutch,
Spanish, Italian, Chinese (Simplified), Polish, Portuguese (Brazil), Swedish and Russian.
**The translations are machine translated.** Some are awkward or wrong, and fixes are welcome.

## Where the translations live

Each language is one file in the repository: `lang/<code>.json` (for example `lang/de.json`,
`lang/zh-CN.json`). `lang/en.json` is the English source; do not translate into it.

```json
{
  "lang": "de",
  "messages": {
    "some.key": ["The German text", "a1b2c3d4"]
  }
}
```

Each message is a key with a two-item list: the translated text, and a fingerprint of the
English text it was translated from. Keep the fingerprint as it is when you fix wording. When
the English text changes, the fingerprint no longer matches, and that message shows in English
until the pack is updated. This is deliberate: an old translation is never shown for new English.

## Rules for the text

AmberSight reads messages in the ICU message format.

- **Placeholders stay unchanged.** `{name}` and `{count}` are filled in by the program. Move them
  within the sentence if the language needs it, but do not translate or rename them.
- **Plurals use the language's own forms.** English has `one` and `other`:

  ```
  {n, plural, one {# event} other {# events}}
  ```

  Use the categories the language has: German, Dutch and Swedish use `one` and `other`.
  Spanish, French, Italian and Portuguese also have `many`, which only matters for numbers like
  1,000,000 and may be left out (`other` is then used). Russian and Polish use `one`, `few`,
  `many` and `other`. Chinese has
  `other` only. `#` stands for the number.
- **A doubled apostrophe `''` is a literal apostrophe.** A single `'` starts quoted text in ICU,
  so write `''` where you want one apostrophe (for example French `l''événement`).
- **HTML tags and entities must match the English exactly**, with the same tags in the same
  order and the same entities (`&amp;`, `&nbsp;`). A message whose HTML differs is ignored and
  the English shows instead. Only the words between the tags are translated.

## Glossary and tone

Terms that must be used the same way everywhere (camera, alert, recording, clip, timeline, and so
on) are listed per language in `tools/i18n/glossary.json` and `tools/i18n/glossary-extra.json`.
Check them before you change a term.

Address the reader informally: German *du*, Spanish *tú*, Italian *tu*, Dutch *je*, Swedish
*du*, Polish *ty*, Portuguese *você*. The exceptions are French, which uses *vous*, and Russian,
which uses *вы*.

## Sending a fix

- Open a pull request on https://github.com/HuskerMinion/AmberSight changing the file in `lang/`; or
- open an issue at https://github.com/HuskerMinion/AmberSight/issues with the key, the current text
  and the text you suggest. The language and, if you can, a screenshot help.

## When a fix reaches you

A fix ships with the next release, never on its own. Each release has the checksum of every
pack fixed into it, and a pack that is not exactly the one built with that release is refused.
After you update AmberSight, the new packs are fetched in the background.
