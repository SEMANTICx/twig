#!/bin/sh
sleep 8
printf "%s" "${GREETING:-}" > out.txt
echo build done
