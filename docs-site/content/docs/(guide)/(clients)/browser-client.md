---
title: Browser Client
description: Stream from a Punktfunk host in a browser tab — turn on browser streaming, run the page on your network, pair once, and play.
---

Stream in a browser tab with nothing to install: you run the page once on your network, and any
browser there opens it. The browser client is a preview; the [native apps](/docs/clients) give the
better picture and the lower latency.

It needs a browser with WebTransport and WebCodecs: current Chrome, Edge, Safari or Firefox. On a
Samsung TV the same page installs as an app: [Samsung TV](/docs/samsung-tv).

## 1. Turn on browser streaming on the host

1. In the host's [web console](/docs/web-console), open **Host** and turn on **Browser
   streaming**.
2. Click **Show advanced** and set **Browser origins** to the addresses you will open the page at,
   for example `https://192.168.1.10:8443`, and its domain if you open it
   [from outside your network](#from-outside-your-network). Left empty, any page open in a browser
   on your network can reach the host. The list is a rule for browser pages: the Samsung TV app
   sends no origin and is admitted either way, and pairing is what lets it stream.
3. Click **Restart Punktfunk**.
4. Open UDP 9778 on the host. On Linux it is in the `punktfunk-native` firewall profile; on
   Windows the host adds its **Punktfunk Browser** rule at the restart ([Ports](/docs/ports)).
   The video goes from the host to the browser directly.

## 2. Run the page

The page runs in a container on any Linux machine on your network; the host itself will do. With
Docker installed:

```sh
curl -O https://raw.githubusercontent.com/punktfunk/client-web/main/deploy/compose.yaml
```

Set `TLS_NAMES` in `compose.yaml` to that machine's address, then start it:

```sh
docker compose up -d
```

The container finds hosts that announce themselves on your network. Settings, under
`environment:` in `compose.yaml`:

| Setting | Default | |
|---|---|---|
| `TLS_NAMES` | — | The names and addresses you open the page at, which its own certificate covers. It answers only to the names listed, to IP addresses and to names no public DNS holds (`localhost`, one word like `desk`, `.local`, `.home.arpa`, `.internal`). With your own certificate and none listed, it answers to any name |
| `PUNKTFUNK_HOSTS` | — | Hosts it can't find: `name=address[:port][#fingerprint]`, comma-separated. A host on this machine is found like any other; to list it, give its LAN address, not `127.0.0.1`, which a browser reads as its own machine |
| `TLS` | `self-signed` | `off` behind a reverse proxy, or `cert.pem,key.pem` |
| `LISTEN` | `0.0.0.0:8443` | Address and port the page is served on |
| `DISCOVER` | on | `0` stops it looking for hosts |
| `ADD_HOSTS` | on | `0` stops it reaching a host typed by address. Either way, it reaches a typed address only once a Punktfunk host has answered there |

## 3. Open it and pair

1. Open `https://<that machine>:8443` and accept the certificate warning once.
2. Pick your host. For one the page did not find, click **Add a host** and type its address.
3. Choose **Request access**, then approve the browser in the console under **Devices → Waiting
   for approval** ([Pairing](/docs/pairing#approve-it-from-the-console-no-pin)). Or click **Pair a
   device** in the console and type its PIN into the page.

Two kinds of request have no **Approve** in the console. Click **Arm PIN** on the row instead, and
type that PIN into the page:

- **Through the web client**: the browser reaches the host through the page's machine, from
  outside your network or where UDP is blocked. A browser on the host's network that connects to it
  directly pairs as above.
- **From a new page**: the page is served from a public name that is not in **Browser origins** and
  that no paired device has used yet. Later devices from that page are approved as usual.

The browser stays paired. Clearing the site's data unpairs it, and a private window is a new device
every time.

## 4. Stream

- Pick a title from the library, or **Stream desktop**. **Resume** continues the title the host
  is running.
- **Menu** in the bar at the top, Ctrl+Alt+Shift+O, or Back+A on a controller opens the quick menu.
  **End stream** closes the title; **Disconnect, keep the game running** leaves it up. The other
  keys are every client's ([Input](/docs/shortcuts#getting-your-input-back)).
- The gear opens **Settings**: stream size, frame rate, the mouse, the statistics overlay. The
  bitrate follows the connection unless you turn **Automatic bitrate** off.

The page uses what the browser offers: HEVC and AV1 where it decodes them, HDR on an HDR display
where it has WebGPU (unless **Video plane** is **WebGL2**), controller rumble where it drives the
pad's motors, and the microphone from the quick menu where it can encode Opus.

## From outside your network

Tailscale is the simplest way. Put the page's machine and your hosts on a tailnet, and download
two files from the [`deploy/` folder](https://github.com/punktfunk/client-web/tree/main/deploy):

```sh
curl -O https://raw.githubusercontent.com/punktfunk/client-web/main/deploy/compose.tailscale.yaml
curl -O https://raw.githubusercontent.com/punktfunk/client-web/main/deploy/tailscale-serve.json
```

In `compose.tailscale.yaml`, set `TLS_NAMES` to the page's tailnet name, and list your hosts in
`PUNKTFUNK_HOSTS` by their tailnet addresses: hosts don't announce themselves across a tailnet.
Start it with your Tailscale auth key:

```sh
TS_AUTHKEY=<your auth key> docker compose -f compose.tailscale.yaml up -d
```

The page then opens at `https://punktfunk.<your tailnet>.ts.net` wherever your devices are on the
tailnet.

Without Tailscale, give the page a domain with a real certificate. `compose.public.yaml` puts Caddy
in front of the page, which gets and renews the certificate:

```sh
curl -O https://raw.githubusercontent.com/punktfunk/client-web/main/deploy/compose.public.yaml
DOMAIN=play.example.com docker compose -f compose.public.yaml up -d
```

Point the domain at your home's public address, and forward TCP 80 and 443 on your router to the
machine that runs the page. Keep your hosts' own ports closed: a browser outside your network
streams through the page's machine, not straight from a host.

Forwarding the port of the setup in step 2 does not work from outside. A browser on the internet
can't tell a certificate the page made itself from an impostor's, so the page shows it a notice
instead of the client.

Someone outside your network who opens the page without a paired browser sees your hosts' names
and can ask for access, a few requests at a time. Nothing more: they can't wake a host, and the
page turns them away for a while when they send too many requests.

## Behind your own reverse proxy

If a reverse proxy with a certificate already runs on your network (Caddy, Traefik, nginx), use
`compose.proxy.yaml` from the `deploy/` folder and point the proxy at `http://<that machine>:8080`.

- Put the proxy's domain in `TLS_NAMES`, and add it to each host's **Browser origins** (step 1).
- Have the proxy pass the browser's `Host` header through unchanged and send `X-Forwarded-For`. In
  nginx:

  ```nginx
  proxy_set_header Host $host;
  proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
  ```

  If the proxy changes `Host`, the page refuses its own requests. Without `X-Forwarded-For`, your
  hosts treat every visitor as coming from the internet.

## Networks that block UDP

Many office and guest networks block UDP, which the stream uses. When the page can't reach the host
over UDP, it switches by itself within a few seconds to a WebSocket connection through the machine
that runs the page, which passes the stream on to the host. There is nothing to set.

This path adds latency, and on a network that loses packets the picture stutters more than over
UDP. The machine that runs the page has to be reachable over HTTPS from the network you are on.
Streaming over this path is a preview, and a browser that pairs over it asks
[through the web client](#3-open-it-and-pair).

## Controller interface

The controller button in the top bar switches the page to Punktfunk Console, the interface every
client draws for a TV and a gamepad: hosts, pairing, the library and settings, all by D-pad.
**Leave console mode** switches back. To start in it, open `https://<that machine>:8443/?ui=console`.

## Links, wake and power

- **Copy a link** on a host card copies a link that opens the page on that host. A link always
  asks before it connects.
- **Wake** on a sleeping host's card, in a browser paired with that host, sends a Wake-on-LAN
  packet from the page's machine, which has to share the host's network
  ([Wake-on-LAN](/docs/wake-on-lan)).
- **Host**, the server button in the library's bar, runs the host's power actions this browser is
  allowed ([Host power](/docs/host-power)) and sends the page's log to the host.

## Troubleshooting

### The page says it can't be opened from the internet

You opened the setup from step 2 from outside your network. Use
[Tailscale or a domain with a real certificate](#from-outside-your-network).

### The page says to add a name to `TLS_NAMES`

You opened the page at a name it doesn't know. Add the name to `TLS_NAMES`, then run
`docker compose up -d` again.

### The page asks you to accept the host's certificate

The page reached the host directly, and the browser does not trust the host's own certificate yet.
Open the link it shows, accept the warning, and choose **I have accepted it**. If it keeps asking,
**Browser streaming** is off on that host (step 1).

### The stream never starts, or drops as it starts

UDP 9778 is blocked between the browser and the host. Open it on the host (step 1) and on any
firewall between them.

### Safari on a Mac never connects

**Auto proxy discovery** is on for the Mac's network connection, and Safari hands the stream to a
proxy. Turn it off in **System Settings → Network → your connection → Details → Proxies**.
