# AmberSight app privacy policy

Last updated: November 21, 2026

The AmberSight app for Android and iPhone lets you watch the cameras on your own AmberSight server. It talks only to the server you add to it. We, the makers of the app, receive nothing from it: no account, no analytics, no ads, no tracking.

## What the app keeps on your phone

- **Server details:** the address of each AmberSight server you add, your user name, and the sign-in token the server gives the app. Passwords and tokens are kept in the phone's protected storage (Android's encrypted storage, or the iPhone keychain).
- **Settings:** your choices in the app, such as layouts, mute and snoozed alerts.

None of this leaves your phone except to sign in to your own server.

## What goes to your AmberSight server

Your server is run by you or by whoever invited you to it. The app sends it only what it needs to show your cameras:

- **Sign-in:** your user name and password, or a sign-in code.
- **Requests:** which cameras, recordings and events to show, and actions you take (for example arm, disarm, mark an event, or move a camera).
- **Talk:** when you hold the Talk button, sound from your microphone is sent to your server and played through that camera's speaker. Nothing is recorded or sent at any other time.
- **Alerts:** on Android, if you turn on alert notifications, the app registers this phone with your server, and alerts are encrypted for this phone and delivered through the UnifiedPush app you choose (for example ntfy). On iPhone the app sends no notifications itself; alerts come through the separate ntfy app, set up on your server.
- **Presence (only in the version installed outside Google Play):** if you turn it on, the app tells your server only whether the phone is home or away, never where it is.

Connections to your server use HTTPS when your server offers it.

## Widgets and Control Center (iPhone)

If you add an AmberSight widget or the Control Center button, the app shares with them, in a keychain area only the app and its widgets can read: your server's addresses, the sign-in token, any certificate you trusted for that server, and your camera list. Your password is never shared. The widgets use this only to fetch a camera picture or the armed state from your own server. This happens only for a server where Stay signed in is on, and stops when you turn it off or remove the server.

## Photos (iPhone)

When you save a clip, the app asks to add it to your Photos library. It can only add; it can't see your other photos.

## Camera

The phone's camera is used only to scan QR codes for signing in or accepting an invite. The picture is read on the phone and not stored or sent.

## Problem reports

If you choose to send a problem report, the app prepares an email in your email app with the app's recent log and device details, with server addresses, names and passwords taken out. Nothing is sent unless you send that email.

## Sharing

We do not sell or share any data. The app contains no advertising or analytics code.

## Children

The app is not designed for children.

## Deleting your data

Everything the app keeps is on your phone and on your own AmberSight server; we hold nothing about you.

- **On your phone:** remove the server in the app, or uninstall the app (on iPhone, its widgets go with it). Either deletes the server details, sign-in token and settings it kept.
- **On your server:** your account, recordings and events are kept by your AmberSight server. Its owner (admin) deletes accounts in Settings > Users & security, and recordings and events in the server itself. If someone else runs the server you use, ask them.
- **Questions or help:** email huskerminion@gmail.com. Since we receive no data from the app, there is nothing for us to delete, but we will help you find it.

## Changes

If this policy changes, the new version will be posted on this page with a new date.

## Contact

Questions about this policy: huskerminion@gmail.com
