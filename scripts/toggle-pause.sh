#!/usr/bin/env bash
set -euo pipefail
set -x

FLAG=pause-transcoding
PROJECT=default
ENV=production

flag_state() {
    ldcli flags get --flag $FLAG --project $PROJECT --env $ENV -o json | python3 -c "
import sys, json
d = json.load(sys.stdin)
e = d['environments']['$ENV']
v = d['variations'][e['fallthrough']['variation'] if e['on'] else e['offVariation']]
print('on:', e['on'], '->', v['name'], '(', v['value'], ')')
"
}

# Current state
flag_state

# Pause
ldcli flags toggle-on --flag $FLAG --project $PROJECT --environment $ENV
flag_state

# Resume
ldcli flags toggle-off --flag $FLAG --project $PROJECT --environment $ENV
flag_state
