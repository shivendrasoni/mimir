# Phase 3 TypeSafe tool-pool shadow experiment

Dataset: `mimir-tool-selection-v1` (32)

## Comparison

| Metric | Full pool | TypeSafe shortlist | Gate |
| --- | ---: | ---: | --- |
| Required-tool recall | 100.0% | 100.0% | at least 100.0% |
| Tool context per provider call | 9450 | 3371.2 tokens | savings at least 60.0% |
| Net first-step savings after Jev input | 0 | 2978.3 tokens/case | at least 500 |
| Selection p95 latency | 0 | 739 ms | at most 2500 ms |
| TypeSafe input | 0 | 3100.4 tokens/case | at most 6000 |
| Full-pool uncertainty fallbacks | n/a | 7 | bounded and safe |
| Estimated selection cost | $0 | $0.004167 total | $0.042/MTok |

Phase 3 gate: **PASS**. Context savings: 64.3%. Missing required tools: 0 of 55.

Jev evaluated every optional tool with an independent Noul in the same request used by skill selection. A tool is included at probability 0.60 or above. Any omitted tool at 0.55 or above makes the runtime keep the full pool. `search_tools`, `search_skills`, and `finish_task` remain available outside the evaluated pool, and `search_tools` can activate an omitted configured tool on the next model step. Net savings charge the complete TypeSafe request against only one provider step; later tool-loop steps increase the savings.

## Cases

| Case | Required | Jev shortlist | Missing after policy | Fallback | Min required p | Max irrelevant p | Tool context | Jev input | Latency (ms) |
| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| none-01 | none | none | none | no | 1.00 | 0.47 | 600 | 3100 | 930 |
| none-02 | none | none | none | no | 1.00 | 0.43 | 600 | 3096 | 401 |
| read-01 | read_file | read_file | none | no | 0.93 | 0.49 | 900 | 3098 | 377 |
| read-02 | read_file | read_file | none | no | 0.95 | 0.54 | 900 | 3100 | 351 |
| list-01 | list_files | list_files | none | full pool | 0.89 | 0.58 | 9450 | 3100 | 374 |
| search-01 | search, read_file | list_files, read_file, search | none | no | 0.89 | 0.67 | 1420 | 3100 | 348 |
| edit-01 | search, read_file, edit_file, run_process | bash, edit_file, list_files, read_file, run_process, search, write_file | none | no | 0.85 | 0.80 | 3170 | 3102 | 370 |
| edit-02 | read_file, edit_file, run_process | bash, edit_file, list_files, read_file, run_process, search | none | full pool | 0.84 | 0.81 | 9450 | 3101 | 375 |
| write-01 | run_process, write_file | bash, list_files, read_file, run_process, search, write_file | none | full pool | 0.79 | 0.80 | 9450 | 3100 | 362 |
| process-01 | run_process | bash, list_files, read_file, run_process, search | none | no | 0.76 | 0.81 | 2200 | 3099 | 348 |
| bash-01 | bash | bash | none | no | 0.83 | 0.52 | 1020 | 3103 | 373 |
| python-01 | list_files, read_file, ipython | ipython, list_files, read_file, search, write_file | none | full pool | 0.83 | 0.77 | 9450 | 3102 | 739 |
| memory-01 | remember | remember | none | no | 0.72 | 0.32 | 780 | 3101 | 451 |
| extension-01 | extension_invoke | edit_file, extension_invoke, list_files, read_file, search, write_file | none | no | 0.88 | 0.74 | 3040 | 3100 | 615 |
| github-01 | github_search_code | github_search_code | none | no | 0.84 | 0.51 | 1120 | 3102 | 391 |
| github-02 | github_create_pull_request | github_create_pull_request | none | no | 0.89 | 0.49 | 1300 | 3100 | 387 |
| github-03 | github_search_code, search, read_file, edit_file, run_process | bash, edit_file, github_search_code, list_files, read_file, run_process, search, write_file | none | no | 0.85 | 0.87 | 3690 | 3104 | 349 |
| linear-01 | linear_search_issues | linear_search_issues | none | no | 0.74 | 0.43 | 1030 | 3100 | 379 |
| linear-02 | linear_create_issue | linear_create_issue | none | no | 0.88 | 0.53 | 1140 | 3098 | 365 |
| linear-03 | search, read_file, linear_create_issue | bash, linear_create_issue, linear_search_issues, list_files, read_file, run_process, search | none | no | 0.86 | 0.83 | 3170 | 3100 | 368 |
| slack-01 | slack_search_messages | slack_search_messages | none | no | 0.89 | 0.43 | 1080 | 3099 | 371 |
| slack-02 | slack_send_message | slack_send_message | none | no | 0.90 | 0.46 | 1160 | 3099 | 380 |
| slack-03 | run_process, slack_send_message | bash, list_files, read_file, run_process, search, slack_search_messages, slack_send_message | none | no | 0.73 | 0.83 | 3240 | 3101 | 547 |
| calendar-01 | calendar_list_events | calendar_list_events | none | no | 0.77 | 0.17 | 980 | 3095 | 366 |
| calendar-02 | calendar_create_event | calendar_create_event, calendar_list_events | none | no | 0.88 | 0.81 | 1480 | 3101 | 363 |
| calendar-03 | slack_search_messages, calendar_create_event | calendar_create_event, calendar_list_events, slack_search_messages | none | full pool | 0.90 | 0.70 | 9450 | 3100 | 366 |
| browser-01 | browser_open_page | browser_click, browser_open_page | none | no | 0.83 | 0.67 | 1460 | 3099 | 330 |
| browser-02 | browser_open_page, browser_click | browser_click, browser_open_page | none | no | 0.86 | 0.49 | 1460 | 3098 | 609 |
| browser-03 | browser_open_page, read_file, edit_file | browser_click, browser_open_page, edit_file, list_files, read_file, search, write_file | none | no | 0.87 | 0.88 | 3250 | 3102 | 356 |
| mixed-01 | linear_search_issues, github_search_code | github_search_code, linear_search_issues, list_files, read_file, search | none | full pool | 0.82 | 0.79 | 9450 | 3107 | 391 |
| mixed-02 | calendar_list_events, slack_send_message | calendar_list_events, slack_send_message | none | no | 0.85 | 0.42 | 1540 | 3100 | 384 |
| mixed-03 | search, read_file, github_search_code, slack_send_message | bash, github_search_code, list_files, read_file, run_process, search, slack_send_message | none | full pool | 0.70 | 0.72 | 9450 | 3107 | 378 |
