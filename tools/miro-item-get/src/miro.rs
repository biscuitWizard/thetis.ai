//! Shared Miro REST API v2 client. Canonical copy; run scripts/sync-miro-client.sh.
#![allow(dead_code)]
use crate::thetis::grip::sys;
use crate::thetis::grip::types::LogLevel;
use serde_json::{json, Value};
use std::{thread, time::Duration};

pub const API_BASE: &str = "https://api.miro.com/v2";
const MAX_RESPONSE: usize = 24_000;

pub struct Miro { token: String, timeout: Duration, retries: u64 }
impl Miro {
 pub fn from_config(raw: &str) -> Result<Self,String> {
  let c: Value=serde_json::from_str(raw).unwrap_or(json!({}));
  let token=["token","api_key"].iter().filter_map(|k|c.get(*k).and_then(Value::as_str)).map(str::trim).find(|s|!s.is_empty()).ok_or_else(||"no Miro token configured. Add [tools.miro]\ntoken = \"...\" to thetis.local.toml; every miro-* tool inherits it".to_string())?.to_string();
  let timeout=c.get("timeout_secs").and_then(Value::as_u64).unwrap_or(30).clamp(5,120);
  let retries=c.get("max_retries").or_else(||c.get("retries")).and_then(Value::as_u64).unwrap_or(2).min(5);
  Ok(Self{token,timeout:Duration::from_secs(timeout),retries})
 }
 pub fn request(&self, method:&str, path:&str, query:&[(String,String)], body:Option<&Value>, safe_retry:bool)->Result<Value,String>{
  let bytes=body.map(|v|v.to_string().into_bytes());
  self.request_bytes(method,path,query,bytes.as_deref(),Some("application/json"),safe_retry)
 }
 pub fn multipart(&self,path:&str,query:&[(String,String)],metadata:&Value,filename:&str,mime:&str,data:&[u8])->Result<Value,String>{
  if data.len()>6*1024*1024{return Err("resource exceeds Miro's 6 MiB upload limit".into())}
  let boundary="----thetis-miro-7MA4YWxkTrZu0gW";
  let mut b=Vec::new();
  b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"data\"\r\nContent-Type: application/json\r\n\r\n{}\r\n",metadata).as_bytes());
  b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"resource\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",safe_filename(filename),mime).as_bytes()); b.extend_from_slice(data); b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
  self.request_bytes("POST",path,query,Some(&b),Some(&format!("multipart/form-data; boundary={boundary}")),false)
 }
 fn request_bytes(&self,method:&str,path:&str,query:&[(String,String)],body:Option<&[u8]>,content_type:Option<&str>,safe_retry:bool)->Result<Value,String>{
  let url=format!("{API_BASE}{path}"); let attempts=if safe_retry {self.retries+1}else{1};
  for attempt in 0..attempts {
   sys::log(LogLevel::Debug,&format!("miro: {method} {path}"));
   let c=waki::Client::new(); let mut r=match method{"GET"=>c.get(&url),"POST"=>c.post(&url),"PATCH"=>c.patch(&url),"PUT"=>c.put(&url),"DELETE"=>c.delete(&url),_=>return Err("unsupported HTTP method".into())};
   r=r.header("Authorization",&format!("Bearer {}",self.token)).header("Accept","application/json").connect_timeout(self.timeout);
   if let Some(ct)=content_type {r=r.header("Content-Type",ct)} if !query.is_empty(){r=r.query(query)} if let Some(b)=body{r=r.body(b.to_vec())}
   let response=match r.send(){Ok(v)=>v,Err(e)=>return Err(format!("could not reach api.miro.com (request was not replayed): {e}"))};
   let status=response.status_code();
   let request_id=response.headers().get("x-request-id").and_then(|v|v.to_str().ok()).unwrap_or("").to_string();
   let retry_after=response.headers().get("retry-after").and_then(|v|v.to_str().ok()).unwrap_or("").to_string();
   let rate_limit=response.headers().get("x-ratelimit-remaining").and_then(|v|v.to_str().ok()).unwrap_or("").to_string();
   let raw=response.body().map_err(|e|format!("could not read Miro response: {e}"))?; let text=String::from_utf8_lossy(&raw).to_string();
   if (200..300).contains(&status){if text.trim().is_empty(){return Ok(json!({}))} return serde_json::from_str(&text).map_err(|e|format!("Miro returned non-JSON: {e}: {}",clip(&text,300)))}
   if safe_retry && (status==429 || status>=500) && attempt+1<attempts {let secs=retry_after.parse::<u64>().unwrap_or(1u64<<attempt).min(10); thread::sleep(Duration::from_secs(secs)); continue}
   return Err(explain_error(status,&text,&request_id,&retry_after,&rate_limit));
  }
  Err("Miro request failed".into())
 }
 pub fn download(&self,url:&str)->Result<Vec<u8>,String>{
  if !url.starts_with("https://api.miro.com/"){return Err("resource_url must use https://api.miro.com/".into())}
  let response=waki::Client::new().get(url).header("Authorization",&format!("Bearer {}",self.token)).connect_timeout(self.timeout).send().map_err(|e|format!("could not download Miro resource: {e}"))?;
  let status=response.status_code();let bytes=response.body().map_err(|e|format!("could not read resource: {e}"))?;if !(200..300).contains(&status){return Err(format!("Miro resource download failed with HTTP {status}"))}Ok(bytes)
 }
 pub fn paginate(&self,path:&str,mut query:Vec<(String,String)>,limit:usize,style:&str)->Result<(Vec<Value>,Option<String>),String>{
  let mut out=Vec::new(); let mut next=query.iter().find(|(k,_)|k==style).map(|(_,v)|v.clone());
  loop {let want=(limit-out.len()).min(50); if want==0{return Ok((out,next))} query.retain(|(k,_)|k!=style&&k!="limit"); query.push(("limit".into(),want.to_string())); if let Some(v)=&next{query.push((style.into(),v.clone()))}
   let v=self.request("GET",path,&query,None,true)?; let rows=v.get("data").or_else(||v.get("items")).and_then(Value::as_array).cloned().unwrap_or_default(); let page_size=rows.len() as u64; out.extend(rows);
   next=v.get("cursor").or_else(||v.get("nextCursor")).or_else(||v.get("next_cursor")).and_then(Value::as_str).map(str::to_string);
   if style=="offset" {let off=v.get("offset").and_then(Value::as_u64).unwrap_or_else(||next.as_deref().and_then(|s|s.parse().ok()).unwrap_or(0)); let size=v.get("size").and_then(Value::as_u64).unwrap_or(page_size); let total=v.get("total").and_then(Value::as_u64).unwrap_or(off+size); let following=off+size; if out.len()>=limit||size==0||following>=total{return Ok((out,None))} next=Some(following.to_string())}
   if next.is_none(){return Ok((out,None))}
  }
 }
}

fn explain_error(status:u16,text:&str,request_id:&str,retry_after:&str,rate:&str)->String{
 let v:Value=serde_json::from_str(text).unwrap_or(json!({}));
 let msg=v.pointer("/message").or_else(||v.pointer("/error/message")).and_then(Value::as_str).unwrap_or(text.trim());
 let code=v.pointer("/code").or_else(||v.pointer("/error/code")).map(|x|x.as_str().map(str::to_string).unwrap_or_else(||x.to_string())).unwrap_or_default();
 let mut details=Vec::new();
 if !code.is_empty(){details.push(format!("code={code}"))} if !request_id.is_empty(){details.push(format!("request-id={request_id}"))} if !retry_after.is_empty(){details.push(format!("retry-after={retry_after}"))} if !rate.is_empty(){details.push(format!("rate-limit-remaining={rate}"))}
 format!("Miro API error {status}: {}{}",clip(msg,800),if details.is_empty(){String::new()}else{format!("; {}",details.join("; "))})
}
pub fn args(raw:&str)->Result<Value,String>{let v:Value=serde_json::from_str(raw).map_err(|e|format!("invalid arguments JSON: {e}"))?; if !v.is_object(){return Err("arguments must be an object".into())} Ok(v)}
pub fn req<'a>(v:&'a Value,k:&str)->Result<&'a str,String>{v.get(k).and_then(Value::as_str).map(str::trim).filter(|s|!s.is_empty()).ok_or_else(||format!("{k} must be a non-empty string"))}
pub fn opt(v:&Value,k:&str)->Option<String>{v.get(k).and_then(Value::as_str).map(str::trim).filter(|s|!s.is_empty()).map(str::to_string)}
pub fn obj(v:&Value,k:&str)->Result<Value,String>{v.get(k).filter(|x|x.is_object()).cloned().ok_or_else(||format!("{k} must be an object"))}
pub fn limit(v:&Value)->usize{v.get("limit").and_then(Value::as_u64).unwrap_or(50).clamp(1,200) as usize}
pub fn q(v:&Value,names:&[&str])->Vec<(String,String)>{names.iter().filter_map(|k|v.get(*k).and_then(|x|if let Some(s)=x.as_str(){let s=s.trim();if s.is_empty(){None}else{Some(s.to_string())}}else if x.is_number()||x.is_boolean(){Some(x.to_string())}else{None}).map(|x|((*k).to_string(),x))).collect()}
pub fn enc(s:&str)->String{s.bytes().map(|b|if b.is_ascii_alphanumeric()||b"-._~".contains(&b){(b as char).to_string()}else{format!("%{b:02X}")}).collect()}
pub fn plural(t:&str)->Result<&'static str,String>{match t{"sticky_note"=>Ok("sticky_notes"),"shape"=>Ok("shapes"),"text"=>Ok("texts"),"card"=>Ok("cards"),"image"=>Ok("images"),"document"=>Ok("documents"),"app_card"=>Ok("app_cards"),"frame"=>Ok("frames"),"embed"=>Ok("embeds"),"preview"=>Ok("previews"),_=>Err("item_type must be sticky_note, shape, text, card, image, document, app_card, frame, embed, or preview".into())}}
pub fn normalized(v:&Value)->String{
 let s=serde_json::to_string_pretty(v).unwrap_or_else(|_|"{}".into());
 if s.len()<=MAX_RESPONSE{return s}
 let continuation="The response exceeded 24000 bytes. Re-run with a lower limit; for list endpoints pass the returned next value as cursor or offset.";
 let mut end=MAX_RESPONSE.min(s.len());
 loop {
  let out=serde_json::to_string_pretty(&json!({"truncated":true,"preview":clip(&s,end),"continuation":continuation})).unwrap_or_else(|_|"{\"truncated\":true}".into());
  if out.len()<=MAX_RESPONSE{return out}
  end=end.saturating_sub(out.len()-MAX_RESPONSE+64);
 }
}
pub fn list_output(rows:Vec<Value>,next:Option<String>)->String{normalized(&json!({"count":rows.len(),"data":rows,"next":next,"note":if next.is_some(){"More results exist; pass next as cursor or offset."}else{"Complete within requested cap."}}))}
pub fn read_file(path:&str)->Result<Vec<u8>,String>{if !path.starts_with("/workspace/"){return Err("file_path must be under /workspace".into())}std::fs::read(path).map_err(|e|format!("cannot read {path}: {e}"))}
pub fn safe_filename(s:&str)->String{let cleaned:String=s.chars().map(|c|if c.is_ascii_alphanumeric()||"._-".contains(c){c}else{'_'}).collect();let cleaned=cleaned.trim_matches('.');if cleaned.is_empty(){"download".into()}else{cleaned.chars().take(180).collect()}}
pub fn save(name:&str,data:&[u8])->Result<String,String>{std::fs::create_dir_all("/workspace/miro").map_err(|e|e.to_string())?;let path=format!("/workspace/miro/{}",safe_filename(name));std::fs::write(&path,data).map_err(|e|e.to_string())?;Ok(path)}
pub fn collect_text(v:&Value,out:&mut Vec<String>){match v{Value::String(s) if !s.trim().is_empty()=>out.push(s.clone()),Value::Array(a)=>for x in a{collect_text(x,out)},Value::Object(o)=>for x in o.values(){collect_text(x,out)},_=>{}}}
pub fn clip(s:&str,n:usize)->String{if s.len()<=n{return s.to_string()}let mut e=n;while !s.is_char_boundary(e){e-=1}format!("{}…",&s[..e])}
