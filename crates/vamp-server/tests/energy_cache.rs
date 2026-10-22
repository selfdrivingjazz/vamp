use std::sync::Arc;
use std::time::Duration;
use vamp_core::Library;
use vamp_protocol::{Metadata, MetadataPatch};

#[test]
fn persisted_pack_relaunch_is_304_and_one_update_is_sparse() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let mut library = Library::init(&home,"main",None,None).unwrap();
    let mut assets = Vec::new();
    let conn = rusqlite::Connection::open(library.state_dir.join("catalog.sqlite3")).unwrap();
    let vector: Vec<u8> = vec![0.5f32;768].iter().flat_map(|v| v.to_le_bytes()).collect();
    for n in 0..40 {
        let path = dir.path().join(format!("{n}.jpg")); std::fs::write(&path,[n]).unwrap();
        let asset = library.add(&path,None,Metadata::default()).unwrap();
        conn.execute("INSERT INTO embeddings(asset_id,source_hash,model,dim,vector) VALUES(?1,?2,'fixture',768,?3)",rusqlite::params![asset.id,asset.original.blake3,vector]).unwrap();
        assets.push(asset);
    }
    let token = vamp_server::token(&library.state_dir).unwrap();
    let app = vamp_server::App::new(library,"main").unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let base = format!("http://{}/v1/embeddings/pack",server.server_addr());
    let serving = { let server=server.clone(); std::thread::spawn(move || vamp_server::serve(server,app,2)) };
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(10))).build().into();
    let auth = format!("Bearer {token}");
    let mut response = agent.get(&base).header("Authorization",&auth).call().unwrap();
    assert_eq!(response.status().as_u16(),200);
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    let bytes = response.body_mut().read_to_vec().unwrap();
    let replica = dir.path().join("replica.bin"); std::fs::write(&replica,&bytes).unwrap();
    let cached = vamp_core::energy::read_header(&replica).unwrap();
    let mut unchanged = agent.get(&base).header("Authorization",&auth).header("If-None-Match",&etag).call().unwrap();
    assert_eq!(unchanged.status().as_u16(),304);
    assert!(unchanged.body_mut().read_to_vec().unwrap().is_empty());
    // Reopening the persisted pack is sufficient; no process-local cache is required.
    assert_eq!(vamp_core::energy::read_header(&replica).unwrap().version,cached.version);
    let mut authority = Library::open(&home,None).unwrap();
    let changed: Vec<u8> = vec![0.8f32;768].iter().flat_map(|v| v.to_le_bytes()).collect();
    conn.execute("INSERT OR REPLACE INTO embeddings(asset_id,source_hash,model,dim,vector) VALUES(?1,?2,'fixture',768,?3)",rusqlite::params![assets[0].id,assets[0].original.blake3,changed]).unwrap();
    authority.update(&assets[0].id,MetadataPatch { description:Some("changed".into()),..MetadataPatch::default() },None).unwrap();
    let mut delta = agent.get(&base).header("Authorization",&auth).header("If-None-Match",&etag)
        .query("after",cached.cursor.to_string()).query("version",&cached.version).query("model",&cached.model)
        .query("dim",cached.dim.to_string()).query("palette_dim",cached.palette_dim.to_string()).call().unwrap();
    assert_eq!(delta.status().as_u16(),200);
    assert_eq!(delta.headers()["content-type"],"application/vnd.vamp.embeddings-delta+json");
    let data=delta.body_mut().read_to_vec().unwrap();
    assert!(data.len()<bytes.len()/10,"delta={} full={}",data.len(),bytes.len());
    let staging=dir.path().join("delta.json");std::fs::write(&staging,&data).unwrap();
    vamp_core::energy::apply_download(&replica,&staging).unwrap();
    let mut complete=agent.get(&base).header("Authorization",&auth).call().unwrap();
    assert_eq!(std::fs::read(&replica).unwrap(),complete.body_mut().read_to_vec().unwrap());
    for _ in 0..2 { server.unblock(); } serving.join().unwrap();
}
