set RUST_LOG=info
curl -X POST "http://localhost:5001/admin/import-key" -H "accept: application/json" -H "Content-Type: application/json-patch+json" -d "[ { \"key\": \"ba5508aba59041f7affe232d5d310aa8\", \"nodeId\": \"0x35ca494ae0085717159de173acd94cf5797a4338\" }]"
cargo run --bin ya-requestor -- --activity-url http://127.0.0.1:6000/activity-api/v1/ --app-key ba5508aba59041f7affe232d5d310aa8 --market-url http://localhost:5001/market-api/v1/