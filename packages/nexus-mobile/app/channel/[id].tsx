/**
 * channel/[id].tsx - Channel chat screen
 */
import React, { useEffect, useRef, useState, useCallback } from "react";
import {
  View, Text, FlatList, TextInput, Pressable, StyleSheet,
  SafeAreaView, KeyboardAvoidingView, Platform, Alert,
  ActivityIndicator, Modal, Clipboard,
} from "react-native";
import { useLocalSearchParams, useRouter } from "expo-router";
import { store, useStore } from "../lib/store";
import type { Message } from "../lib/api";

const EMOJI_SHORTCUTS = ["👍", "❤️", "😂", "😮", "😢", "🔥"];

function formatTime(iso: string) {
  const d = new Date(iso);
  const now = new Date();
  const diff = now.getTime() - d.getTime();
  if (diff < 86400000) return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (diff < 604800000) return d.toLocaleDateString([], { weekday: "short", hour: "2-digit", minute: "2-digit" });
  return d.toLocaleDateString();
}

interface MessageItemProps {
  msg: Message;
  onReact: (emoji: string) => void;
  onReply: () => void;
  onLongPress: () => void;
}

function MessageItem({ msg, onReact, onReply, onLongPress }: MessageItemProps) {
  const [showReactions, setShowReactions] = useState(false);
  const isOwn = msg.authorId === store.session?.user.id;

  return (
    <Pressable
      onLongPress={() => { setShowReactions(false); onLongPress(); }}
      style={isOwn ? styles.myMessage : styles.otherMessage}
    >
      {!isOwn && (
        <View style={styles.avatar}>
          <Text style={styles.avatarText}>{msg.authorUsername[0]?.toUpperCase()}</Text>
        </View>
      )}
      <View style={styles.messageContent}>
        {!isOwn && (
          <Text style={styles.authorName}>{msg.authorUsername}</Text>
        )}
        {msg.replyTo && (
          <View style={styles.replyIndicator}>
            <Text style={styles.replyText}>Replying to a message</Text>
          </View>
        )}
        {msg.content ? <Text style={styles.messageText}>{msg.content}</Text> : null}
        {msg.editedAt ? <Text style={styles.editedLabel}>(edited)</Text> : null}
        {msg.embeds?.map((e, i) => (
          <View key={i} style={styles.embed}>
            {e.title ? <Text style={styles.embedTitle}>{e.title}</Text> : null}
            {e.description ? <Text style={styles.embedDesc} numberOfLines={3}>{e.description}</Text> : null}
            {e.url ? <Text style={styles.embedUrl}>{e.url}</Text> : null}
          </View>
        ))}
        {msg.reactions && msg.reactions.length > 0 && (
          <View style={styles.reactions}>
            {msg.reactions.map((r, i) => (
              <Pressable key={i} onPress={() => onReact(r.emoji)} style={[styles.reaction, r.me && styles.reactionMe]}>
                <Text style={styles.reactionEmoji}>{r.emoji}</Text>
                <Text style={[styles.reactionCount, r.me && styles.reactionCountMe]}>{r.count}</Text>
              </Pressable>
            ))}
          </View>
        )}
        <View style={styles.messageFooter}>
          <Text style={styles.timestamp}>{formatTime(msg.createdAt)}</Text>
          <Pressable onPress={() => { setShowReactions(v => !v); }} style={styles.emojiBtn}>
            <Text style={styles.emojiBtnText}>+</Text>
          </Pressable>
        </View>
      </View>

      {showReactions && (
        <View style={[styles.quickReactions, isOwn && styles.quickReactionsLeft]}>
          {EMOJI_SHORTCUTS.map(emoji => (
            <Pressable key={emoji} onPress={() => { onReact(emoji); setShowReactions(false); }} style={styles.quickReaction}>
              <Text style={styles.quickReactionText}>{emoji}</Text>
            </Pressable>
          ))}
        </View>
      )}
    </Pressable>
  );
}

function TypingIndicator({ channelId }: { channelId: string }) {
  const typing = store.typingUsers[channelId];
  if (!typing || typing.length === 0) return null;
  const text = typing.length === 1
    ? typing[0] + " is typing..."
    : typing.slice(0, 2).join(", ") + " are typing...";
  return <Text style={styles.typingIndicator}>{text}</Text>;
}

export default function ChannelScreen() {
  useStore();
  const { id } = useLocalSearchParams<{ id: string }>();
  const router = useRouter();
  const [input, setInput] = useState("");
  const typingTimerRef = React.useRef<ReturnType<typeof setTimeout> | null>(null);
  const [replyTo, setReplyTo] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const flatListRef = useRef<FlatList>(null);

  // Message context menu
  const [contextMsg, setContextMsg] = useState<Message | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editInput, setEditInput] = useState("");
  const [editLoading, setEditLoading] = useState(false);

  const channel = store.channels.find(c => c.id === id) ?? store.dmChannels.find(c => c.id === id);
  const messages = (id ? store.messages[id] : []) ?? [];

  useEffect(() => {
    if (id && store.activeChannelId !== id) {
      store.selectChannel(id).catch(console.error);
    }
  }, [id]);

  async function handleSend() {
    if (!input.trim()) return;
    const content = input.trim();
    setInput("");
    setLoading(true);
    try {
      await store.sendMessage(content, replyTo ?? undefined);
      setReplyTo(null);
    } catch (e: any) {
      Alert.alert("Error", e.message);
    } finally {
      setLoading(false);
    }
  }

  async function handleReact(messageId: string, emoji: string) {
    const msg = messages.find(m => m.id === messageId);
    if (!msg || !id) return;
    const existing = msg.reactions?.find(r => r.emoji === emoji);
    if (existing?.me) {
      await store.removeReaction(id, messageId, emoji);
    } else {
      await store.addReaction(id, messageId, emoji);
    }
  }

  function handleLoadMore() {
    if (id) store.loadOlderMessages().catch(console.error);
  }

  function openContext(msg: Message) {
    setContextMsg(msg);
  }

  function closeContext() {
    setContextMsg(null);
  }

  function handleContextReply() {
    if (!contextMsg) return;
    setReplyTo(contextMsg.id);
    closeContext();
  }

  function handleContextCopy() {
    if (!contextMsg) return;
    Clipboard.setString(contextMsg.content ?? "");
    closeContext();
  }

  function handleContextEdit() {
    if (!contextMsg) return;
    setEditingId(contextMsg.id);
    setEditInput(contextMsg.content ?? "");
    closeContext();
  }

  async function handleContextDelete() {
    if (!contextMsg) return;
    closeContext();
    Alert.alert(
      "Delete message",
      "This cannot be undone.",
      [
        { text: "Cancel", style: "cancel" },
        {
          text: "Delete", style: "destructive",
          onPress: async () => {
            try {
              await store.api.deleteMessage(contextMsg.id);
              if (id) store.onMessageDelete(id, contextMsg.id);
            } catch (e: any) {
              Alert.alert("Error", e.message);
            }
          },
        },
      ],
    );
  }

  async function handleSaveEdit() {
    if (!editingId || !editInput.trim()) return;
    setEditLoading(true);
    try {
      const updated = await store.api.editMessage(editingId, editInput.trim());
      if (id) store.onMessageUpdate(updated);
      setEditingId(null);
      setEditInput("");
    } catch (e: any) {
      Alert.alert("Error", e.message);
    } finally {
      setEditLoading(false);
    }
  }

  const isOwn = (msg: Message) => msg.authorId === store.session?.user.id;

  const renderMessage = useCallback(({ item }: { item: Message }) => (
    <MessageItem
      msg={item}
      onReact={(emoji) => handleReact(item.id, emoji)}
      onReply={() => setReplyTo(item.id)}
      onLongPress={() => openContext(item)}
    />
  ), [messages]);

  const channelName = channel
    ? ("name" in channel ? (channel as any).name ?? "DM" : "DM")
    : "Channel";

  return (
    <SafeAreaView style={styles.container}>
      {/* Header */}
      <View style={styles.header}>
        <Pressable onPress={() => router.back()} style={styles.backBtn}>
          <Text style={styles.backBtnText}>Back</Text>
        </Pressable>
        <View style={styles.headerTitleArea}>
          <Text style={styles.headerTitle}>{channelName}</Text>
          {channel && "topic" in channel && (channel as any).topic ? (
            <Text style={styles.headerTopic} numberOfLines={1}>{(channel as any).topic}</Text>
          ) : null}
        </View>
        {channel && "isE2ee" in channel && (channel as any).isE2ee ? (
          <Text style={styles.e2eeBadge}>E2EE</Text>
        ) : null}
      </View>

      {/* Messages */}
      <FlatList
        ref={flatListRef}
        data={[...messages].reverse()}
        keyExtractor={item => item.id}
        renderItem={renderMessage}
        contentContainerStyle={styles.messageList}
        inverted
        onEndReached={handleLoadMore}
        onEndReachedThreshold={0.3}
        ListEmptyComponent={
          <View style={styles.emptyChannel}>
            <Text style={styles.emptyChannelText}>
              {channelName === "DM" ? "No messages yet" : "This is the beginning of #" + channelName}
            </Text>
          </View>
        }
      />

      <TypingIndicator channelId={id ?? ""} />

      {/* Reply bar */}
      {replyTo && !editingId && (
        <View style={styles.replyBar}>
          <Text style={styles.replyBarText}>
            Replying to: {messages.find(m => m.id === replyTo)?.authorUsername ?? "message"}
          </Text>
          <Pressable onPress={() => setReplyTo(null)}>
            <Text style={styles.replyBarClose}>✕</Text>
          </Pressable>
        </View>
      )}

      {/* Edit bar */}
      {editingId && (
        <View style={styles.editBar}>
          <Text style={styles.editBarLabel}>Editing message</Text>
          <Pressable onPress={() => { setEditingId(null); setEditInput(""); }}>
            <Text style={styles.replyBarClose}>✕</Text>
          </Pressable>
        </View>
      )}

      {/* Composer */}
      <KeyboardAvoidingView
        behavior={Platform.OS === "ios" ? "padding" : "height"}
        keyboardVerticalOffset={90}
      >
        <View style={styles.composer}>
          <View style={styles.inputRow}>
            <TextInput
              style={styles.input}
              placeholder={editingId ? "Edit message…" : (channelName !== "DM" ? ("Message #" + channelName) : "Message…")}
              placeholderTextColor="#7890b0"
              value={editingId ? editInput : input}
              onChangeText={editingId ? setEditInput : (text) => {
                setInput(text);
                if (!typingTimerRef.current && id) {
                  store.sendTyping(id).catch(() => {});
                  typingTimerRef.current = setTimeout(() => {
                    typingTimerRef.current = null;
                  }, 4_000);
                }
              }}
              multiline
              maxLength={4000}
            />
            <Pressable
              onPress={editingId ? handleSaveEdit : handleSend}
              style={[styles.sendBtn, editingId && styles.editBtn]}
              disabled={(editingId ? editLoading || !editInput.trim() : loading || !input.trim())}
            >
              {(editingId ? editLoading : loading) ? (
                <ActivityIndicator color="#062020" size="small" />
              ) : (
                <Text style={styles.sendBtnText}>{editingId ? "Save" : "Send"}</Text>
              )}
            </Pressable>
          </View>
        </View>
      </KeyboardAvoidingView>

      {/* Message context menu */}
      <Modal visible={!!contextMsg} transparent animationType="fade" onRequestClose={closeContext}>
        <Pressable style={styles.contextOverlay} onPress={closeContext}>
          <View style={styles.contextMenu}>
            <Text style={styles.contextPreview} numberOfLines={2}>
              {contextMsg?.content || "📎 Attachment"}
            </Text>
            <View style={styles.contextDivider} />

            <Pressable style={styles.contextItem} onPress={handleContextReply}>
              <Text style={styles.contextItemText}>Reply</Text>
            </Pressable>
            <Pressable style={styles.contextItem} onPress={handleContextCopy}>
              <Text style={styles.contextItemText}>Copy text</Text>
            </Pressable>
            {contextMsg && isOwn(contextMsg) && (
              <>
                <Pressable style={styles.contextItem} onPress={handleContextEdit}>
                  <Text style={styles.contextItemText}>Edit</Text>
                </Pressable>
                <Pressable style={styles.contextItem} onPress={handleContextDelete}>
                  <Text style={[styles.contextItemText, styles.contextItemDanger]}>Delete</Text>
                </Pressable>
              </>
            )}
          </View>
        </Pressable>
      </Modal>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: "#030a14" },
  header: { flexDirection: "row", alignItems: "center", padding: 10, backgroundColor: "#071327", borderBottomWidth: 1, borderBottomColor: "#0f2040" },
  backBtn: { paddingRight: 10 },
  backBtnText: { color: "#27c9a5", fontSize: 15, fontWeight: "600" },
  headerTitleArea: { flex: 1 },
  headerTitle: { color: "#d6f4ff", fontSize: 16, fontWeight: "700" },
  headerTopic: { color: "#7890b0", fontSize: 12 },
  e2eeBadge: { backgroundColor: "#27c9a5", color: "#062020", fontSize: 10, fontWeight: "700", paddingHorizontal: 6, paddingVertical: 2, borderRadius: 4, overflow: "hidden" },
  messageList: { paddingHorizontal: 8, paddingVertical: 4 },
  emptyChannel: { flex: 1, justifyContent: "center", alignItems: "center", paddingTop: 100 },
  emptyChannelText: { color: "#7890b0", fontSize: 14 },
  myMessage: { flexDirection: "row", justifyContent: "flex-end", marginBottom: 4 },
  otherMessage: { flexDirection: "row", marginBottom: 4 },
  avatar: { width: 32, height: 32, borderRadius: 16, backgroundColor: "#0b1a30", justifyContent: "center", alignItems: "center", marginRight: 8 },
  avatarText: { color: "#9ec5d5", fontSize: 14, fontWeight: "600" },
  messageContent: { maxWidth: "75%", backgroundColor: "#0b1a30", borderRadius: 12, paddingHorizontal: 12, paddingVertical: 8 },
  authorName: { color: "#9ec5d5", fontSize: 12, fontWeight: "600", marginBottom: 2 },
  replyIndicator: { borderLeftWidth: 2, borderLeftColor: "#27c9a5", paddingLeft: 6, marginBottom: 4 },
  replyText: { color: "#7890b0", fontSize: 11, fontStyle: "italic" },
  messageText: { color: "#d6f4ff", fontSize: 15, lineHeight: 20 },
  editedLabel: { color: "#7890b0", fontSize: 10, fontStyle: "italic", marginTop: 2 },
  embed: { backgroundColor: "#112544", borderRadius: 8, padding: 10, marginTop: 6 },
  embedTitle: { color: "#27c9a5", fontWeight: "600", fontSize: 14, marginBottom: 4 },
  embedDesc: { color: "#9ec5d5", fontSize: 13 },
  embedUrl: { color: "#7890b0", fontSize: 11, marginTop: 4 },
  reactions: { flexDirection: "row", flexWrap: "wrap", marginTop: 6, gap: 4 },
  reaction: { flexDirection: "row", alignItems: "center", backgroundColor: "#112544", borderRadius: 12, paddingHorizontal: 8, paddingVertical: 3 },
  reactionMe: { backgroundColor: "#1e3a5f" },
  reactionEmoji: { fontSize: 14 },
  reactionCount: { color: "#7890b0", fontSize: 12, marginLeft: 4 },
  reactionCountMe: { color: "#27c9a5" },
  messageFooter: { flexDirection: "row", alignItems: "center", justifyContent: "flex-end", marginTop: 4, gap: 6 },
  timestamp: { color: "#7890b0", fontSize: 10 },
  emojiBtn: { paddingHorizontal: 4 },
  emojiBtnText: { color: "#7890b0", fontSize: 14, fontWeight: "700" },
  quickReactions: { position: "absolute", bottom: -36, right: 0, flexDirection: "row", backgroundColor: "#0b1a30", borderRadius: 20, padding: 4, gap: 4, zIndex: 10 },
  quickReactionsLeft: { right: undefined, left: 0 },
  quickReaction: { padding: 4 },
  quickReactionText: { fontSize: 18 },
  typingIndicator: { color: "#7890b0", fontSize: 12, fontStyle: "italic", paddingHorizontal: 12, paddingVertical: 2 },
  replyBar: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", backgroundColor: "#0f2040", paddingHorizontal: 12, paddingVertical: 6 },
  replyBarText: { color: "#9ec5d5", fontSize: 13, flex: 1 },
  replyBarClose: { color: "#7890b0", fontSize: 14, fontWeight: "700", paddingLeft: 8 },
  editBar: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", backgroundColor: "#112544", paddingHorizontal: 12, paddingVertical: 6 },
  editBarLabel: { color: "#27c9a5", fontSize: 13, fontWeight: "600", flex: 1 },
  composer: { backgroundColor: "#071327", paddingHorizontal: 8, paddingVertical: 8, borderTopWidth: 1, borderTopColor: "#0f2040" },
  inputRow: { flexDirection: "row", alignItems: "flex-end", backgroundColor: "#112544", borderRadius: 20, paddingHorizontal: 14, paddingVertical: 8 },
  input: { flex: 1, color: "#d6f4ff", fontSize: 15, maxHeight: 120 },
  sendBtn: { marginLeft: 8, backgroundColor: "#27c9a5", borderRadius: 14, paddingHorizontal: 14, paddingVertical: 6 },
  editBtn: { backgroundColor: "#7c6af7" },
  sendBtnText: { color: "#062020", fontWeight: "700", fontSize: 14 },
  // Context menu
  contextOverlay: { flex: 1, backgroundColor: "rgba(0,0,0,0.6)", justifyContent: "center", alignItems: "center", padding: 20 },
  contextMenu: { backgroundColor: "#0b1a30", borderRadius: 16, width: "100%", overflow: "hidden" },
  contextPreview: { color: "#9ec5d5", fontSize: 13, padding: 16, fontStyle: "italic" },
  contextDivider: { height: 1, backgroundColor: "#0f2040" },
  contextItem: { paddingHorizontal: 20, paddingVertical: 16 },
  contextItemText: { color: "#d6f4ff", fontSize: 16 },
  contextItemDanger: { color: "#ed4245" },
});
