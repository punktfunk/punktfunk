# tvOS — Privacy Policy text

App Store Connect asks for a **URL** on iOS and macOS but for **text** on tvOS: the Apple TV has
no browser, so the App Store shows the policy itself on the device. The field is required for the
tvOS platform, localisable per language, and ships with the next version like any other metadata.
Apple documents no character limit; `check-limits.py` holds it under 4000 like a description.

Paste the block for each language into **App Information → Privacy Policy for Apple TV**. It is
the short form of [`privacy-app-addendum.md`](privacy-app-addendum.md): the app collects nothing,
so it says so, plus the controller and complaint line a standalone policy needs. Keep the two in
step.

## Deutsch (1136)

```
Datenschutzerklärung für Punktfunk auf Apple TV

Verantwortlicher: Enrico Bühler, unom, Schroffenstraße 44, 78628 Rottweil, Deutschland. E-Mail: buehler@unom.io

Die App erhebt keine personenbezogenen Daten. Es gibt kein Konto, kein Tracking, keine Analyse-, Werbe- oder Absturzbericht-Bibliotheken, und es werden keine Daten an uns oder an Dritte übermittelt.

Punktfunk verbindet Ihr Apple TV direkt mit einem Host-Rechner, den Sie selbst betreiben. Video, Ton und Eingaben laufen verschlüsselt nur zwischen diesen beiden Geräten, ohne einen Server von uns. Das lokale Netzwerk dient allein dazu, Hosts zu finden, zu wecken und zu verbinden.

Hosts, Einstellungen und der Schlüssel, mit dem sich Ihr Apple TV gegenüber einem gekoppelten Host ausweist, bleiben auf dem Gerät und werden mit der App gelöscht.

Da wir nichts über Sie speichern, gibt es bei uns nichts einzusehen, zu berichtigen oder zu löschen. Fragen: buehler@unom.io. Beschwerden: Landesbeauftragter für den Datenschutz und die Informationsfreiheit Baden-Württemberg (Art. 77 DSGVO).

Vollständige Fassung: unom.io/legal/privacy#punktfunk-apps. Stand: 5. November 2026
```

## English (1029)

```
Privacy Policy for Punktfunk on Apple TV

Controller: Enrico Bühler, unom, Schroffenstraße 44, 78628 Rottweil, Germany. Email: buehler@unom.io

The app collects no personal data. There is no account, no tracking, no analytics, advertising, or crash-reporting library, and no data is sent to us or to any third party.

Punktfunk connects your Apple TV directly to a host machine you run yourself. Video, audio, and input travel encrypted between those two devices only, with no server of ours in between. The local network is used solely to find, wake, and connect to hosts.

Hosts, settings, and the key your Apple TV uses to identify itself to a paired host stay on the device and are deleted with the app.

Since we store nothing about you, there is nothing held by us to access, correct, or delete. Questions: buehler@unom.io. Complaints: State Commissioner for Data Protection and Freedom of Information of Baden-Württemberg (Art. 77 GDPR).

Full policy: unom.io/en/legal/privacy#punktfunk-apps. Last updated: 5 November 2026
```
