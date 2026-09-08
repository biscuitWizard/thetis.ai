wit_bindgen::generate!({world:"tool",path:"../../wit",generate_all});
mod miro;
use miro::Miro;
use serde_json::{json,Value};
struct Component;
impl Guest for Component {
 fn describe()->ToolManifest { ToolManifest {
  name:"miro-board-member-share".into(),
  description:"Invite 1-20 members to a Miro board by email using the v2 board-members endpoint.".into(),
  args_schema_json:json!({
   "type":"object",
   "properties":{
    "board_id":{"type":"string"},
    "emails":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"string"}},
    "role":{"type":"string","enum":["viewer","commenter","editor","coowner","owner"]},
    "message":{"type":"string"}
   },
   "required":["board_id","emails"],
   "additionalProperties":false
  }).to_string(),
  capabilities:vec!["http".into(),"boards:write".into(),"group:miro".into()]
 }}
 fn invoke(_:String,a:String,c:String)->Result<String,String> {
  let a=miro::args(&a)?;
  let id=miro::req(&a,"board_id")?;
  let emails=a.get("emails").and_then(Value::as_array).ok_or("emails must be an array")?;
  if emails.is_empty()||emails.len()>20{return Err("emails must contain 1-20 entries".into())}
  for (i,email) in emails.iter().enumerate(){if email.as_str().map(str::trim).filter(|s|!s.is_empty()).is_none(){return Err(format!("emails[{i}] must be a non-empty string"))}}
  let mut body=json!({"emails":emails});
  if let Some(role)=a.get("role"){body["role"]=role.clone()}
  if let Some(message)=a.get("message"){body["message"]=message.clone()}
  let path=format!("/boards/{}/members",miro::enc(id));
  Ok(miro::normalized(&Miro::from_config(&c)?.request("POST",&path,&[],Some(&body),false)?))
 }
}
export!(Component);
