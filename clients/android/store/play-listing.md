# Google Play listing

Play Console copy that has to match the app. `ci/play-upload.py` uploads bundles only. The
listing text for en-US, de-DE, es-ES and fr-FR is set from a file, where the store images are
composed.

## Accessibility service

Punktfunk uses the AccessibilityService API (`KeyCaptureService`) and is not an accessibility
tool, so `isAccessibilityTool` stays unset. Play then requires three things at every submission:

1. The full description says what the API is used for (below).
2. A prominent disclosure in normal use, not only in Settings. `KeyCaptureDisclosure` shows it
   once at launch when a hardware keyboard is attached, and Settings → Input → Keyboard shortcuts
   shows it again. Keep its text and the paragraph below saying the same thing.
3. A current video link in the Accessibility API declaration
   (App content → Sensitive permissions and APIs).

### Full description paragraph

Append to the full description (4000-character cap), in every language the listing has.

English:

> Keyboard shortcuts (AccessibilityService API): Android keeps Alt+Tab, the Windows key and your
> keyboard's language key for itself. To send them to the computer you stream from, Punktfunk can
> use Android's AccessibilityService API. It is optional and off until you turn it on: the app
> asks once when a hardware keyboard is attached, and Settings → Input → Keyboard shortcuts offers
> it any time. With it on, Punktfunk receives the keys you press on a hardware keyboard and sends
> them to that computer only while a stream is on screen. Outside a stream every key passes
> through untouched. The service can't see the screen or other apps' content, and stores nothing.

German:

> Tastenkürzel (AccessibilityService-API): Android behält Alt+Tab, die Windows-Taste und die
> Sprachtaste deiner Tastatur für sich. Damit sie den Rechner erreichen, von dem du streamst, kann
> Punktfunk die AccessibilityService-API von Android nutzen. Das ist freiwillig und aus, bis du es
> einschaltest: Die App fragt einmal, wenn eine Hardware-Tastatur angeschlossen ist, und unter
> Einstellungen → Eingabe → Tastenkürzel jederzeit. Ist es an, empfängt Punktfunk die Tasten deiner
> Hardware-Tastatur und schickt sie nur während eines laufenden Streams an diesen Rechner. Außerhalb
> eines Streams gehen alle Tasten unverändert durch. Der Dienst sieht weder den Bildschirm noch
> Inhalte anderer Apps und speichert nichts.

Spanish:

> Atajos de teclado (API AccessibilityService): Android se reserva Alt+Tab, la tecla Windows y la
> tecla de idioma de tu teclado. Para enviarlas al equipo desde el que transmites, Punktfunk puede
> usar la API AccessibilityService de Android. Es opcional y está desactivada hasta que la actives:
> la app pregunta una vez cuando hay un teclado físico conectado, y Ajustes → Entrada → Atajos de
> teclado la ofrece en cualquier momento. Con ella activada, Punktfunk recibe las teclas que pulsas
> en un teclado físico y las envía a ese equipo solo mientras hay una transmisión en pantalla.
> Fuera de una transmisión, todas las teclas pasan sin cambios. El servicio no puede ver la
> pantalla ni el contenido de otras apps, y no guarda nada.

French:

> Raccourcis clavier (API AccessibilityService) : Android garde pour lui Alt+Tab, la touche Windows
> et la touche de langue de votre clavier. Pour les envoyer à l’ordinateur depuis lequel vous
> diffusez, Punktfunk peut utiliser l’API AccessibilityService d’Android. C’est facultatif et
> désactivé tant que vous ne l’activez pas : l’app pose la question une fois lorsqu’un clavier
> physique est connecté, et Paramètres → Entrées → Raccourcis clavier le propose à tout moment.
> Une fois activé, Punktfunk reçoit les touches que vous pressez sur un clavier physique et les
> envoie à cet ordinateur uniquement lorsqu’un flux est à l’écran. En dehors d’un flux, toutes
> les touches passent sans modification. Le service ne peut voir ni l’écran ni le contenu des
> autres apps, et n’enregistre rien.

### Declaration answers

- Accessibility tool: no.
- What it does: forwards system-reserved keyboard shortcuts (Alt+Tab, Windows key, language key)
  from a hardware keyboard to the user's own computer during a remote-desktop stream. It requests
  key filtering only (`flagRequestFilterKeyEvents`), only while a stream is on screen, no events
  and no window content.

### Review video

One take on a phone or tablet with a hardware keyboard and a paired host. Reset the one-time
prompt first with `adb shell pm clear io.unom.punktfunk`. Record every step:

1. Launch the app. The prompt appears over the home screen. Hold on the full text.
2. Tap No thanks. The app keeps working; start a stream and show Alt+` standing in for Alt+Tab.
3. Open Settings → Input → Keyboard shortcuts. The same prompt appears. Tap Agree.
4. Android's Accessibility page opens. Turn on Punktfunk keyboard shortcuts and accept Android's
   own dialog.
5. Start a stream and press Alt+Tab. The host's task switcher opens and Android's recents do not.
6. Leave the stream and press Alt+Tab again. Android's own switcher opens: nothing is captured
   outside a stream.
