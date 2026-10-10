#!/bin/sh
# Renders the Windows wizard's brand rasters (reactor's image element is raster-only):
#   lens-highlight.png             the mark's overlap path, cropped to the mark's box
#   wordmark-{dark,light}-{0..3}.png  the four "funk" letters, one per file, same canvas
# Geometry is the website's (pfweb Wordmark.tsx / BrandMark.tsx = web/public/favicon.svg):
# two circles r=194.41 at (403.037,597.262)/(597.808,402.853) in a 1000 box, letters in
# 0 0 579 136. The mark itself is drawn live as two ellipses; only the non-elliptical parts
# are rasters. Needs rsvg-convert (brew install librsvg). Run from the repo root, commit.
set -eu
out=crates/install/punktfunk-setup-win/assets
mkdir -p "$out"

overlap='M647.84,590.737c-64.853,17.403 -136.871,0.597 -187.885,-50.416c-51.013,-51.013 -67.819,-123.032 -50.416,-187.885c64.853,-17.403 136.871,-0.597 187.885,50.416c51.013,51.013 67.819,123.032 50.416,187.885Z'
# The mark's bounding box: light circle's left/bottom edge to the deep circle's right/top.
printf '<svg xmlns="http://www.w3.org/2000/svg" viewBox="208.627 208.443 583.776 583.776"><path d="%s" fill="#d2c9fb"/></svg>' "$overlap" \
  | rsvg-convert -w 480 -h 480 -o "$out/lens-highlight.png"

# The letters, verbatim from pfweb's Wordmark.tsx (Export/Punktfunk_Logo-Text_No-Border_Dark.svg).
f='M16.782,16.051l0,102.687l31.253,0l0,-35.563l73.436,0l0,-23.555l-73.436,0l0,-19.398l77.285,0l0,-24.171l-108.537,0Z'
u='M131.785,16.051l0,47.264c0.154,16.627 0.154,16.627 0.308,20.014c0.77,15.087 2.463,21.4 7.544,26.634c7.698,8.16 20.014,10.315 59.272,10.315c23.863,0 34.178,-0.616 43.415,-2.463c11.7,-2.463 19.552,-10.623 21.246,-22.169c0.77,-5.542 1.078,-12.316 1.232,-31.868l0,-47.727l-31.253,0l0,47.264c-0.462,15.703 -0.462,15.703 -0.616,19.706c-0.616,10.007 -2.617,14.163 -8.006,16.627c-4.618,2.155 -10.777,2.771 -26.634,2.771c-30.021,0 -33.87,-1.847 -35.563,-16.319c-0.462,-4.926 -0.616,-8.006 -0.77,-22.785l0,-47.264l-31.253,0Z'
n='M271.575,15.943l0,102.687l31.868,0l-0.77,-76.669l3.387,0l54.038,76.669l54.346,0l0,-102.687l-31.868,0l0.77,76.515l-3.233,0l-53.73,-76.515l-54.808,0Z'
k='M420.91,15.943l0,102.687l31.253,0l0,-39.258l17.089,0l46.032,39.258l47.418,0l-64.353,-52.344l59.426,-50.959l-47.88,0l-40.644,37.873l-17.089,0l0,-37.257l-31.253,0Z'
i=0
for d in "$f" "$u" "$n" "$k"; do
  for scheme in dark light; do
    case $scheme in dark) fill='#d2c9fb';; light) fill='#6c5bf3';; esac
    printf '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 579 136"><path d="%s" fill="%s"/></svg>' "$d" "$fill" \
      | rsvg-convert -w 1158 -h 272 -o "$out/wordmark-$scheme-$i.png"
  done
  i=$((i + 1))
done
ls -la "$out"
