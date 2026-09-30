#!/bin/sh
RATE="1 1000 2000 3000 4000"
DURATION="60s"

HTTP_TUNNEL="http://127.0.0.1:5202"
FRP="http://127.0.0.1:5203"

echo warming up frp
echo GET $FRP | vegeta attack -duration 10s > /dev/null
for rate in $RATE; do
        name="frp-${rate}qps-$DURATION.bin"
        echo $name
        echo GET $FRP | vegeta attack -rate $rate -duration $DURATION > $name
        vegeta report $name
done

echo warming up http-tunnel
echo GET $HTTP_TUNNEL | vegeta attack -duration 10s > /dev/null
for rate in $RATE; do
        name="http-tunnel-${rate}qps-$DURATION.bin"
        echo $name
        echo GET $HTTP_TUNNEL | vegeta attack -rate $rate -duration $DURATION > $name
        vegeta report $name
done
