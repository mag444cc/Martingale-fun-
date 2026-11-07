#!/bin/bash
# Download Binance USD-M futures 1-minute candles for the backtest.
#
#   scripts/get-data.sh SOLUSDT 2021-01 2022-12
#
# Puts unzipped CSV files in data/SOLUSDT/ (ignored by git). One month of
# one coin is about 2 MB to download, 4 MB unzipped.
set -euo pipefail
symbol=${1:?usage: scripts/get-data.sh SYMBOL FROM_MONTH TO_MONTH, e.g. SOLUSDT 2021-01 2022-12}
from=${2:?first month, e.g. 2021-01}
to=${3:?last month, e.g. 2022-12}
base=https://data.binance.vision/data/futures/um/monthly/klines/$symbol/1m
out=data/$symbol
mkdir -p "$out"

month=$from
while [[ "$month" < "$to" || "$month" == "$to" ]]; do
  file=$symbol-1m-$month.zip
  if [ ! -f "$out/$symbol-1m-$month.csv" ]; then
    echo "downloading $file"
    if curl -fsS -o "$out/$file" "$base/$file"; then
      unzip -oq "$out/$file" -d "$out" && rm "$out/$file"
    else
      echo "  (not there: before the coin was listed, or the month isn't over yet)"
      rm -f "$out/$file"
    fi
  fi
  # next month
  y=${month%-*}; m=$((10#${month#*-} + 1))
  if [ $m -gt 12 ]; then m=1; y=$((y + 1)); fi
  month=$(printf "%d-%02d" "$y" "$m")
done
echo "done: $(ls "$out"/*.csv 2>/dev/null | wc -l) months in $out"
